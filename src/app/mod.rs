//! iced `daemon` app for the DualSense battery tray.
//!
//! The daemon boots windowless: it owns the tray icon and only opens windows on
//! demand (controller popup, configure window, overlay toast). The toast window
//! is pre-created hidden after the tray appears so iced keeps a warm GPU
//! compositor; popup, Settings, and Launcher stay ephemeral (create/destroy on each
//! open — Launcher uses a normal OS window open).

mod actions;
mod browser;
mod configure;
mod feedback;
mod home;
pub(crate) mod installs;
mod launcher;
mod osk;
mod popup;
mod shell;
mod toast;
mod update;

use crate::controller::dualsense::lightbar::ClaimStore;
use crate::domain::color::SpectrumService;
use crate::domain::pad::{InputHub, PadAction, PadActionGate};
use crate::{AppError, AppResult};

use crate::browser::engine::{EngineCommand, EngineEvent, EngineHandle};
use crate::browser::keys::RawKey;
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
use crate::controller::model::{ControllerStatus, Serial};
use crate::domain::pad::ChordReleaseDetector;
use crate::games::GamesCatalog;
use crate::games::process_match::RunningSession;
use crate::games::process_match::SeenCache;
use crate::games::steam::SteamGame;
use crate::persist::analytics::AnalyticsStore;
use crate::persist::prefs::{NavbarVisibility, Prefs};
use crate::persist::updater::UpdaterState;
use crate::platform::app_log;
use crate::platform::host::Host;
use crate::platform::ui_sound::{SoundHub, UiSoundKind};
use crate::platform::updater::UpdateStatus;
use crate::session::notify::NotifyEvent;
use crate::ui::configure::{
    self as configure_view, AnalyticsPanel, ConfigureMessage, ConfigureState, PadInputPanel,
    Section,
};
use crate::ui::cursor_hide;
use crate::ui::launcher::art_worker::ArtOwner;
use crate::ui::launcher::input::{
    self as launcher_input, CrossHold, FaceHeld, NavAction, NavLogSnapshot, NavProfile, NavStepper,
};
use crate::ui::launcher::view::{self as launcher_view, LauncherMessage};
use crate::ui::layout::{MonitorCover, ToastPlacement, TrayAnchor, focus_window};
use crate::ui::popup::{self as popup_view, ControllerRow, PopupMessage};
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
use iced::{Element, Fill, Size, Subscription, Task, Theme, stream, window};

use std::collections::{HashMap, HashSet, VecDeque};
use std::future::Future;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant};

#[cfg(not(windows))]
use tray_icon::TrayIcon;

/// Debounce spectrum prefs save + HID apply after the last color edit.
const SPECTRUM_DEBOUNCE: Duration = Duration::from_millis(150);
/// Debug builds: comma list of windows to open at boot (see [`Message::DebugOpen`]).
#[cfg(debug_assertions)]
const DEBUG_OPEN_ENV: &str = crate::platform::debug_quiet::DEBUG_OPEN_ENV;
/// How long an overlay toast stays on screen.
const TOAST_LIFETIME: Duration = Duration::from_secs(5);
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
/// Closed-Launcher cadence for the running-badge wall-clock tick (open Launcher
/// refreshes at `RUNNING_CHECK_INTERVAL` through pad polls already). Slower
/// keeps background process scans cheap during real gameplay.
const RUNNING_CHECK_CLOSED_INTERVAL: Duration = Duration::from_secs(5);
const PAD_POLL_STALL_MS: u128 = 80;
const PAD_POLL_SLOW_MS: u128 = 20;
const SNAPSHOT_STALE_MS: u128 = 100;
const LAUNCHER_NAV_NOT_READY_MS: u128 = 200;
/// Ignore Launcher WindowUnfocused briefly after reveal (toast z-order focus blips).
const LAUNCHER_UNFOCUS_GRACE: Duration = Duration::from_millis(150);
/// Hide the native cursor after this idle on Home/Browser/Launcher. One shell
/// cursor state spans all three screens, so swapping screens never re-shows it.
const SHELL_CURSOR_IDLE_HIDE: Duration = Duration::from_secs(2);
/// Kick Steam scan if Launcher never opened / settled (tray-only boot).
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
    PollResult(AppResult<Vec<ControllerStatus>>),
    /// Result of a background process-image snapshot (running badge).
    ProcessEnumResult(AppResult<Vec<(u32, PathBuf)>>),
    /// Steam `Apps` registry changed (see `platform::steam_watch`): re-read
    /// the session verdict now instead of waiting for the next badge tick.
    SteamAppsChanged,
    /// Wall-clock tick for the running-game badge. Owns live sessions while
    /// Launcher is closed / pad-idle so a failed launch cannot freeze `Playing`
    /// until the next open.
    RunningBadgeTick,
    /// Background game-close finished (WM_CLOSE + terminate); badge refresh clears "Closing".
    CloseGameDone(AppResult<()>),

    /// A tray menu item was activated.
    TrayMenu(String),
    /// The tray icon was left-clicked at the given screen rectangle.
    TrayLeftClick(TrayAnchor),

    WindowClosed(window::Id),
    WindowUnfocused(window::Id),
    WindowFocused(window::Id),

    PopupOpened(window::Id),
    PlacePopup {
        id: window::Id,
        scale: f32,
        monitor: Option<Size>,
    },
    Popup(PopupMessage),

    ConfigureOpened(window::Id),
    Configure(ConfigureMessage),
    /// Browser-window chrome / page / cursor message.
    Browser(crate::ui::browser::BrowserMessage),
    /// Poll a pending history jump that never reported landing/error.
    BrowserHistoryTimeout,
    /// Debug-only: 5s Browser frame-shot cadence + settled-dock capture.
    #[cfg(debug_assertions)]
    BrowserDebugShotTick,
    /// System OSK overlay window opened.
    OskOpened(window::Id),
    /// System OSK overlay message (no interactive keys; reserved).
    Osk(crate::ui::osk::OskMessage),
    /// Debug-only: capture the OSK overlay after the settle delay.
    #[cfg(debug_assertions)]
    OskCapture,
    /// Debug-only: OSK overlay window screenshot ready to persist.
    #[cfg(debug_assertions)]
    OskShot(iced::window::Screenshot),
    /// Optional BrowserEngine install/update finished or was cancelled.
    EngineInstallDone {
        generation: u64,
        outcome: crate::app::browser::EngineInstallOutcome,
    },
    /// Circle/Esc or the Cancel row while an install/update is in flight.
    EngineInstallCancel,
    /// Live BrowserEngine install phase (download/verify/extract).
    #[cfg(feature = "cef-engine")]
    EngineInstallProgress {
        generation: u64,
        progress: crate::browser::engine_install::InstallProgress,
    },
    /// Background DLC manifest check finished (Home open).
    #[cfg(feature = "cef-engine")]
    EngineManifestReady {
        generation: u64,
        result: Result<Option<crate::browser::engine_install::EngineManifest>, String>,
    },
    /// Background Info-panel size walk finished.
    EngineInfoSize {
        generation: u64,
        result: Result<(u64, u64), String>,
    },
    /// BrowserEngine uninstall finished.
    #[cfg(feature = "cef-engine")]
    EngineUninstallDone {
        generation: u64,
        result: Result<(), String>,
    },
    /// 60Hz tick while Home animates the indeterminate uninstall meter.
    HomeProgressTick,
    /// 60Hz tick while a Home controller card expands/collapses on focus.
    HomeControllerTick,
    /// 60Hz tick while a Home hold ring is charging/decaying.
    HomeFrame,
    /// A window resized (logical size). The browser and Home consume this;
    /// the event is authoritative for their final full-cover size.
    WindowSized(window::Id, iced::Size),
    /// Hidden shell setup is done: show the window and begin the backdrop
    /// fade (the card is visible immediately; the scrim fades in after).
    WindowShowNow(window::Id),
    /// The shared shell HWND finished creating (renderer is up).
    ShellOpened(window::Id),
    /// Shell is shown offscreen with the entrance pose; present it on the
    /// monitor after one paint.
    ShellPresent(window::Id, u64),
    /// Offscreen prime ran; arm the first-redraw reveal gate.
    ShellRevealArmed(window::Id, u64),
    /// First shell redraw after the offscreen prime (`listen_raw`; the
    /// filtered window-event listener intentionally drops RedrawRequested).
    ShellRedraw(window::Id),
    /// Safety net: re-present/re-focus the shell if the first present was lost.
    ShellFocusFallback(window::Id, u64),
    /// 60Hz redraw while a Home/Browser backdrop fade is running.
    ShellFadeTick,
    /// PS Home window opened (focus it once the renderer exists).
    /// Home selection / activation from pad, keyboard, or mouse.
    Home(crate::ui::home::HomeMessage),
    /// Raw keyboard event for the Browser app (controller-first: no mouse).
    /// Routed in `update` — page mode forwards to the engine, URL-edit mode
    /// and reader mode fall back to the legacy Launcher-key mapping.
    BrowserRawKey {
        id: window::Id,
        key: RawKey,
    },
    /// Platform IME preedit/commit for the Browser page.
    BrowserIme {
        id: window::Id,
        event: iced::advanced::input_method::Event,
    },
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

    LauncherOpened(window::Id),
    /// Drive launcher animation frames.
    LauncherFrame,
    /// Low-rate idle timer (sleep/dim) while no animation needs frames.
    LauncherIdleTick,
    /// Low-rate shell cursor clock: hides the native pointer after idle on any
    /// of Home/Browser/Launcher, and restores it when all three are gone.
    ShellCursorTick,
    /// Mouse move/button/wheel on a window — resets shell cursor and start activity.
    ShellCursorActivity(window::Id),
    /// Single-flight art worker milestone (selected / near / job done).
    LauncherArtEvent(crate::ui::launcher::art_worker::ArtEvent),
    Launcher(LauncherMessage),
    /// CEF engine worker event (title / URL / load / DOM). System-class:
    /// frames flow continuously while visible and must not count as user
    /// activity (or idle-dim and cursor-hide would never fire).
    BrowserEngine(EngineEvent),
    /// Keyboard while some iced window has focus; filtered to the launcher in update.
    LauncherKey {
        id: window::Id,
        action: LauncherKeyAction,
        pressed: bool,
    },
    /// Pad input edge from the hid-worker (report-rate; replaces UI PadPoll timer).
    PadInput(crate::domain::pad::InputEdge),
    ManualFilePicked(Option<PathBuf>),
    ManualIconPicked(Option<PathBuf>),
    SteamPathPicked(Option<PathBuf>),
    SteamScanDone(AppResult<Vec<SteamGame>>),
    /// Boot idle fallback: start deferred Steam scan if still not kicked.
    SteamScanIdleFallback,
    /// Debug-only window stress driver (Diagnostics section in Settings):
    /// rapid toast + cold Launcher + configure + edit churn to shake out
    /// renderer (e.g. wgpu atlas) crashes. Takes over windows while running.
    #[cfg(debug_assertions)]
    StressWindows(u8),
    /// Debug-only: open the windows named in `SDSC_DEBUG_OPEN` after boot
    /// (`popup`, `settings[:section]`, `start`, `toast`; comma-separated) so
    /// UI work can be screenshotted without clicking through the tray.
    #[cfg(debug_assertions)]
    DebugOpen(String),
    /// Internal replay-driver tick (only armed by `--trace-replay`).
    ReplayStep,
    /// Debug replay: one window presented a frame while a settle gate waited.
    ReplayFrame,
    /// Debug replay: event-driven settle timed out for this wait generation.
    ReplayWaitTimeout(u64),
    /// Debug replay: navigation grace elapsed for this wait generation.
    ReplayWaitGrace(u64),
    /// Debug replay: a screenshot finished writing to disk.
    ReplayCaptureDone,
    /// Debug-only: flip the layout-bounding-box overlay (Diagnostics toggle or F9).
    #[cfg(debug_assertions)]
    ToggleDebugLayoutBounds,
    /// Debug-only: browser target overlay + frame shots (Diagnostics toggle
    /// or F10). Mirrored into the browser panel for the card readout.
    #[cfg(debug_assertions)]
    ToggleDebugBrowser,

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
    /// Fire the queued renderer reload (short feedback delay after the click).
    RendererReloadDue,
    /// Slow utility-ambient drift tick (≤2fps, only while a utility window is
    /// visible — popup, Settings, or non-immersive Launcher). Toasts excluded.
    UtilityAmbientTick,
    /// Begin dismiss (slide-out) for the toast of the given generation.
    ToastDismiss(u64),
    /// Hide toast HWND after a short defer (avoid create_renderer vs hide stress).
    ToastHideDeferred,

    /// Debounced spectrum prefs save + lightbar HID apply.
    SpectrumCommit(u64),

    /// Delayed boot update check fired (see `UPDATE_BOOT_DELAY`).
    BootUpdateCheck,
    /// Result of a background GitHub release fetch.
    UpdateCheckDone(AppResult<crate::platform::updater::ReleaseInfo>),
    /// Native update notification delivered (fire-and-forget).
    UpdateNotifyDone(AppResult<()>),

    Exit,
}

fn input_trace(
    label: &'static str,
    surface: &'static str,
) -> crate::platform::trace::ActionBuilder {
    crate::platform::trace::action(crate::platform::trace::Lane::App, "app.input")
        .source("ui")
        .field("message", label)
        .field("surface", surface)
}

impl Message {
    /// Semantic trace entry for user-originated messages. `PadInput` is
    /// handled at the pad-action boundary; system/background messages return
    /// `None`. This match is exhaustive on purpose: a new user-facing variant
    /// must be classified here before it compiles.
    fn trace_action(&self) -> Option<crate::platform::trace::ActionBuilder> {
        use crate::platform::trace;
        if !trace::actions_enabled() {
            return None;
        }
        match self {
            Message::Tick
            | Message::PresenceHint
            | Message::AbsenceRecheck
            | Message::PollResult(_)
            | Message::ProcessEnumResult(_)
            | Message::SteamAppsChanged
            | Message::RunningBadgeTick
            | Message::CloseGameDone(_)
            | Message::WindowClosed(_)
            | Message::WindowUnfocused(_)
            | Message::WindowFocused(_)
            | Message::PopupOpened(_)
            | Message::PlacePopup { .. }
            | Message::ConfigureOpened(_)
            | Message::BrowserHistoryTimeout
            | Message::OskOpened(_)
            | Message::Osk(_)
            | Message::EngineInstallDone { .. }
            | Message::EngineInfoSize { .. }
            | Message::HomeProgressTick
            | Message::HomeControllerTick
            | Message::HomeFrame
            | Message::WindowSized(_, _)
            | Message::WindowShowNow(_)
            | Message::ShellOpened(_)
            | Message::ShellPresent(_, _)
            | Message::ShellRevealArmed(_, _)
            | Message::ShellRedraw(_)
            | Message::ShellFocusFallback(_, _)
            | Message::ShellFadeTick
            | Message::LauncherOpened(_)
            | Message::LauncherFrame
            | Message::LauncherIdleTick
            | Message::ShellCursorTick
            | Message::ShellCursorActivity(_)
            | Message::LauncherArtEvent(_)
            | Message::BrowserEngine(_)
            | Message::SteamScanDone(_)
            | Message::SteamScanIdleFallback
            | Message::PlaceToast { .. }
            | Message::ToastShown { .. }
            | Message::ToastFrame
            | Message::IdentifyFrame
            | Message::RendererReloadDue
            | Message::UtilityAmbientTick
            | Message::ToastDismiss(_)
            | Message::ToastHideDeferred
            | Message::SpectrumCommit(_)
            | Message::BootUpdateCheck
            | Message::UpdateCheckDone(_)
            | Message::UpdateNotifyDone(_)
            | Message::ReplayStep
            | Message::Exit
            | Message::PadInput(_) => None,
            Message::ReplayFrame
            | Message::ReplayWaitTimeout(_)
            | Message::ReplayWaitGrace(_)
            | Message::ReplayCaptureDone => None,
            #[cfg(debug_assertions)]
            Message::EmulatorCommitTick
            | Message::AmbientPreviewOpened(_)
            | Message::AmbientPreviewTick
            | Message::BrowserDebugShotTick
            | Message::OskCapture
            | Message::OskShot(_) => None,
            #[cfg(feature = "cef-engine")]
            Message::EngineInstallProgress { .. }
            | Message::EngineManifestReady { .. }
            | Message::EngineUninstallDone { .. } => None,
            Message::TrayMenu(id) => Some(input_trace("tray.menu", "tray").field("menu", id)),
            Message::TrayLeftClick(_) => Some(input_trace("tray.click", "tray")),
            Message::Popup(_) => Some(input_trace("ui.popup", "popup")),
            Message::Configure(_) => Some(input_trace("ui.settings", "settings")),
            Message::Browser(message) => message
                .trace_label()
                .map(|label| input_trace(label, "browser")),
            Message::BrowserRawKey { .. } => Some(input_trace("ui.key", "browser")),
            Message::BrowserIme { .. } => None,
            Message::Home(message) => {
                let mut trace = input_trace(message.trace_label(), "home");
                if let crate::ui::home::HomeMessage::AppsMenuOpen(index) = message {
                    trace = trace.field("index", *index);
                }
                Some(trace)
            }
            Message::Launcher(message) => message
                .trace_label()
                .map(|label| input_trace(label, "launcher")),
            Message::LauncherKey {
                action, pressed, ..
            } => Some(
                input_trace("ui.key", "launcher")
                    .field("key_action", format!("{action:?}"))
                    .field("pressed", *pressed),
            ),
            Message::ManualFilePicked(path) => Some(
                input_trace("dialog.file", "settings")
                    .field("path", path.as_ref().map(|p| p.display().to_string())),
            ),
            Message::ManualIconPicked(path) => Some(
                input_trace("dialog.icon", "settings")
                    .field("path", path.as_ref().map(|p| p.display().to_string())),
            ),
            Message::SteamPathPicked(path) => Some(
                input_trace("dialog.steam_path", "settings")
                    .field("path", path.as_ref().map(|p| p.display().to_string())),
            ),
            Message::EngineInstallCancel => Some(input_trace("engine.install.cancel", "home")),
            #[cfg(debug_assertions)]
            Message::StressWindows(count) => {
                Some(input_trace("debug.stress", "debug").field("count", *count))
            }
            #[cfg(debug_assertions)]
            Message::DebugOpen(open) => {
                Some(input_trace("debug.open", "debug").field("windows", open))
            }
            #[cfg(debug_assertions)]
            Message::ToggleDebugLayoutBounds => Some(input_trace("debug.toggle.layout", "debug")),
            #[cfg(debug_assertions)]
            Message::ToggleDebugBrowser => Some(input_trace("debug.toggle.browser", "debug")),
            #[cfg(debug_assertions)]
            Message::PreviewDrag | Message::PreviewClose => {
                Some(input_trace("debug.preview", "debug"))
            }
        }
    }

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
            Message::Tick
            | Message::PresenceHint
            | Message::AbsenceRecheck
            | Message::ReplayStep => false,
            Message::ReplayFrame
            | Message::ReplayWaitTimeout(_)
            | Message::ReplayWaitGrace(_)
            | Message::ReplayCaptureDone => false,
            #[cfg(debug_assertions)]
            Message::EmulatorCommitTick | Message::BrowserDebugShotTick => false,
            Message::PollResult(_)
            | Message::ProcessEnumResult(_)
            | Message::RunningBadgeTick
            | Message::SteamAppsChanged
            | Message::CloseGameDone(_) => false,
            Message::TrayMenu(id) => id.as_str() != QUIT_ID,
            Message::TrayLeftClick(_) => true,
            Message::WindowClosed(_)
            | Message::WindowUnfocused(_)
            | Message::WindowFocused(_)
            | Message::PopupOpened(_)
            | Message::PlacePopup { .. }
            | Message::ConfigureOpened(_)
            | Message::LauncherOpened(_)
            | Message::LauncherFrame
            | Message::LauncherIdleTick
            | Message::ShellCursorTick
            | Message::BrowserHistoryTimeout
            | Message::OskOpened(_)
            | Message::EngineInstallDone { .. }
            | Message::EngineInfoSize { .. }
            | Message::WindowSized(_, _)
            | Message::WindowShowNow(_)
            | Message::ShellOpened(_)
            | Message::ShellPresent(_, _)
            | Message::ShellRevealArmed(_, _)
            | Message::ShellRedraw(_)
            | Message::ShellFocusFallback(_, _)
            | Message::ShellFadeTick
            | Message::LauncherArtEvent(_) => false,
            #[cfg(feature = "cef-engine")]
            Message::EngineInstallProgress { .. }
            | Message::EngineManifestReady { .. }
            | Message::EngineUninstallDone { .. } => false,
            Message::HomeProgressTick | Message::HomeControllerTick | Message::HomeFrame => false,
            #[cfg(debug_assertions)]
            Message::AmbientPreviewOpened(_) | Message::AmbientPreviewTick => false,
            #[cfg(debug_assertions)]
            Message::OskCapture | Message::OskShot(_) => false,
            #[cfg(debug_assertions)]
            Message::PreviewDrag | Message::PreviewClose => true,
            Message::Popup(_)
            | Message::Configure(_)
            | Message::Home(_)
            | Message::EngineInstallCancel
            | Message::Browser(_)
            | Message::Launcher(_)
            | Message::BrowserRawKey { .. }
            | Message::BrowserIme { .. }
            | Message::ShellCursorActivity(_)
            | Message::Osk(_)
            | Message::PadInput(_) => true,
            Message::LauncherKey { .. } => true,
            Message::ManualFilePicked(_)
            | Message::ManualIconPicked(_)
            | Message::SteamPathPicked(_) => true,
            Message::SteamScanDone(_) | Message::SteamScanIdleFallback => false,
            Message::BrowserEngine(_) => false,
            #[cfg(debug_assertions)]
            Message::StressWindows(_) => true,
            #[cfg(debug_assertions)]
            Message::DebugOpen(_) => true,
            #[cfg(debug_assertions)]
            Message::ToggleDebugLayoutBounds => true,
            #[cfg(debug_assertions)]
            Message::ToggleDebugBrowser => true,
            Message::PlaceToast { .. }
            | Message::ToastShown { .. }
            | Message::ToastFrame
            | Message::IdentifyFrame
            | Message::RendererReloadDue
            | Message::UtilityAmbientTick
            | Message::ToastDismiss(_)
            | Message::ToastHideDeferred
            | Message::SpectrumCommit(_)
            | Message::BootUpdateCheck
            | Message::UpdateCheckDone(_)
            | Message::UpdateNotifyDone(_) => false,
            Message::Exit => false,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LauncherKeyAction {
    Up,
    Down,
    Left,
    Right,
    Confirm,
    Cancel,
}

/// Surface currently entitled to source-side button actions.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum PadInputOwner {
    #[default]
    None,
    Home,
    Browser,
    Launcher,
}

pub struct App {
    session: crate::session::DeviceSession,
    games: GamesCatalog,
    /// In-progress launcher edit checklist; committed on Save, discarded on Cancel.
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
    /// Owned pad-input Home shared with the HID worker thread and the
    /// pad-input subscription (no process-global).
    input_hub: Arc<InputHub>,
    /// Owned lightbar-claim unit shared with the HID worker (presence-empty
    /// fast path syncs through this clone; no process-global).
    claims: Arc<ClaimStore>,
    /// Owned Launcher-art decode unit (single-flight mailbox + iced event sink;
    /// no process-global).
    art: Arc<ArtOwner>,
    /// Owned UI-sound worker handle (lazy worker spawn on first play;
    /// no process-global).
    sound: SoundHub,

    /// Windows: handle to the dedicated tray thread (icon, menu, and modal
    /// menu loop live there so a stuck menu cannot wedge the iced thread).
    /// Other platforms own tray-icon directly (their toolkits require it).
    #[cfg(windows)]
    tray_icon: Option<tray::TrayHandle>,
    #[cfg(not(windows))]
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
    /// immersive"): immersive-rate background without opening Launcher.
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
    /// Debug-only browser overlay + frame shots (Diagnostics toggle / F10).
    /// Local UI state: never persisted. Mirrored into the browser panel.
    #[cfg(debug_assertions)]
    pub debug_browser: bool,
    /// Debug-only ambient pulse + ken-burns tuning (Diagnostics sliders).
    /// Local UI state: never persisted; defaults
    /// reproduce release visuals. Mirrored into the process-wide tuning
    /// atomics (`ui::shader::ambient`, `ui::launcher::mode`) on every edit.
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

    launcher_window: Option<window::Id>,
    /// True only while Launcher is revealed (warm HWND may exist while false).
    launcher_visible: bool,
    /// One physical fullscreen shell window shared by Home/Browser/Launcher.
    /// The three `*_window` fields above are aliases for the active mode (or
    /// None while the shell is hidden for PS); this id stays warm so PS
    /// hide/resume never destroys/recreates the surface.
    shell_window: Option<window::Id>,
    shell_mode: home::ShellScreen,
    /// The shell window exists but is hidden (PS dismiss). The next open shows
    /// it again instead of creating a new HWND.
    shell_hidden: bool,
    /// Created hidden; `WindowShowNow` configures size/level and presents it.
    shell_pending_show: bool,
    /// A mode switch requested while the outgoing card is still animating out;
    /// `finish_*_close` consumes it and switches in place.
    pending_shell_mode: Option<home::ShellScreen>,
    /// Monotonic token for the last shell show/hide request. Raw Win32
    /// visibility effects can run out of order; stale tokens no-op.
    shell_visibility_epoch: std::sync::Arc<std::sync::atomic::AtomicU64>,
    /// Standalone fullscreen Browser app window.
    browser_window: Option<window::Id>,
    browser_visible: bool,
    /// Monitor covered by the Browser window (stage size for its view).
    browser_monitor_cover: Option<MonitorCover>,
    /// Actual logical window size reported by iced after creation/resize.
    browser_window_size: Option<(f32, f32)>,
    /// Browser window entrance/exit (card scale/opacity/y), same language as
    /// Home.
    browser_shell_anim: Option<crate::ui::home::ShellAnim>,
    /// Launcher entrance/exit (black-veil fade + card movement). The Launcher
    /// keeps its own ceremony; this only wraps the shared shell transition.
    launcher_shell_anim: Option<crate::ui::home::ShellAnim>,
    /// One dim backdrop for the one shell window.
    shell_backdrop: crate::ui::motion::ShellBackdrop,
    /// Topmost PS app-switcher overlay: a full-screen dim scrim with the
    /// card centered in it.
    home_window: Option<window::Id>,
    /// True while Home is revealed; modal owner of pad input.
    home_visible: bool,
    /// Screen PS dismissed and the next PS restores. `None` = next PS opens
    /// Home. Circle/tile transitions clear it so a stale screen never resurfaces.
    ps_resume: Option<home::ShellScreen>,
    /// A PS resume was requested while the Launcher window was still closing;
    /// the WindowClosed path retries it.
    ps_resume_pending: bool,
    /// Actual logical size of Home window (always the monitor cover).
    home_stage: (f32, f32),
    /// Home window entrance/exit (card scale/opacity/y + scrim fade).
    home_shell_anim: Option<crate::ui::home::ShellAnim>,
    /// Monitor covered by Home window (toast composite pose).
    home_monitor_cover: Option<MonitorCover>,
    home_state: crate::ui::home::State,
    /// Last Launcher-window paint (for the transition paint-gap witness;
    /// `Cell` because `view()` only gets `&self`).
    last_start_paint_at: std::cell::Cell<Option<Instant>>,
    /// Whether that paint happened with a transition ceremony live (gates the
    /// witness so idle gaps never log).
    last_start_paint_active: std::cell::Cell<bool>,
    /// False until Launcher is revealed and ready — blocks pad nav / sounds early.
    launcher_nav_ready: bool,
    launcher_state: launcher_view::State,
    /// Top-level browser app: panel, engine worker, capability flag, and
    /// browser-only D-pad stepper. No longer owned by `LauncherState`.
    browser: browser::BrowserAppState,
    /// System controller keyboard overlay (owned by the OS, never the Browser).
    osk: osk::SystemOsk,
    /// Home-owned standalone install policy (download cap shared by every
    /// managed-app installer).
    install_policy: installs::AppInstallPolicy,
    /// Managed-app Home: installed/active apps and transition effects.
    home_apps: home::HomeApps,
    /// Monitor covered by the Launcher window (stage size for the view).
    launcher_monitor_cover: Option<MonitorCover>,
    /// This process was relaunched by the crash handler: restore tabs in
    /// safe mode (default live tab, saved tabs stored).
    crash_restart_boot: bool,
    /// Last user activity on Launcher (mouse / keyboard / pad) for idle/sleep.
    launcher_last_activity: Instant,
    /// Last shell-screen (Home/Browser/Launcher) mouse activity; one clock for
    /// all three so a screen swap does not re-show the pointer.
    shell_cursor_last_active: Instant,
    /// When the PS Home opened over a visible Launcher. On Home close, Launcher's
    /// animation stamps and idle clock are shifted forward by this pause so
    /// the frozen splash art resumes instead of snapping ahead.
    launcher_home_pause: Option<Instant>,
    /// When true, every shell view forces [`mouse::Interaction::Hidden`].
    shell_cursor_hidden: bool,
    /// A shell screen is opening (Home/Browser/Launcher). Keeps the cursor
    /// hidden across a swap before the replacement window exists.
    shell_opening: bool,
    /// The current show sequence already ran its present/focus pass.
    shell_presented: bool,
    /// Offscreen prime finished; wait for the shell's first redraw event
    /// before moving the warm HWND onscreen (REV-1).
    shell_reveal_armed: bool,
    /// Generation guard for delayed reveal timers: a fallback from an older
    /// show must never present the next one before its prime paints.
    shell_reveal_generation: u64,
    /// Entrance clock for the first-present diagnostic.
    shell_reveal_started: Option<Instant>,
    /// True while an rfd picker is open from the launcher (suppress unfocus-close).
    launcher_file_dialog_open: bool,
    /// PS fires on release only; a reset consumes the current hold.
    ps_release: ChordReleaseDetector,
    /// Face-button release latch after Home closes on Circle/Cross. The
    /// Home consumed the press; without this the same hold reaches the app
    /// underneath and dismisses its popup/window. Time-bounded so a missed
    /// release sample can never wedge pad input.
    home_face_latch_until: Option<Instant>,
    /// When Launcher was last revealed (unfocus grace).
    launcher_revealed_at: Option<Instant>,
    pad_nav: crate::domain::navigation::NavDispatcher,
    /// Source-action delivery gate (sequence dedupe + owner arming).
    pad_actions: PadActionGate,
    /// Source taps waiting for a pad frame to run through the surface handler.
    /// Edges can conflate during a UI stall; draining one per input frame keeps
    /// every tap without changing the one-action surface contracts.
    pending_pad_actions: VecDeque<PadAction>,
    /// Browser history/nav action waiting for the engine to settle.
    pending_browser_trace: Option<crate::app::browser::PendingBrowserTrace>,
    /// Multi-step history jump, advanced one CEF step per canonical snapshot.
    pending_history_travel: Option<crate::app::browser::PendingHistoryTravel>,
    /// In-process replay plan when launched with `--trace-replay`.
    replay: Option<crate::platform::trace::replay::ReplayPlan>,
    /// Event-driven replay settle gate.
    replay_wait: Option<crate::app::browser::ReplayWait>,
    replay_wait_seq: u64,
    /// Screenshot captures scheduled by replay and not yet handled.
    replay_capture_pending: u32,
    /// Replay screenshot retries while the current page paint is still old.
    #[cfg(debug_assertions)]
    replay_capture_retries: u32,
    /// The plan has no more steps; one final engine settle runs before done.
    replay_finished: bool,
    /// The first replay step has waited for any boot/default page load.
    replay_prime_done: bool,
    /// Surface receiving source-side tap actions.
    pad_input_owner: PadInputOwner,
    /// R2 rising-edge latch while the manual-shortcut form is open (R2 saves).
    manual_add_r2_latched: bool,
    /// When set, the next start cue may rumble this DualSense serial (pad-originated).
    haptic_pad_serial: Option<Serial>,
    /// Keyboard Enter hold for replace-confirm (not folded into pad slots).
    keyboard_cross_hold: CrossHold,
    /// Keyboard Enter held (for hold-to-proceed on replace confirm / Cross press styling).
    confirm_key_held: bool,
    /// Keyboard Escape held (Circle press styling).
    cancel_key_held: bool,
    /// Last armed-pad face held snapshot (merged with keyboard in [`Self::sync_launcher_held`]).
    pad_held: FaceHeld,
    running_session: Option<RunningSession>,
    /// Steam appids whose install dir failed pre-flight (missing/empty):
    /// launch routes to View in Steam until the next successful scan proves
    /// the install back (which clears this set).
    launch_blocked_steam: HashSet<u32>,
    /// Match paths for the target currently showing "Closing" (cleared on process exit).
    closing_match_paths: Vec<PathBuf>,
    /// Throttle for process enumeration / catalog restore (not pad UI).
    last_running_check: Option<Instant>,
    /// True while a background process enum task is outstanding.
    process_enum_inflight: bool,
    /// Cached `match_paths_for_target` results (cleared on Steam library refresh).
    match_path_cache: HashMap<String, Vec<PathBuf>>,
    /// First-sight cache for missing Steam installdir warnings (never
    /// cleared; later sights stay silent for the process lifetime).
    seen_cache: SeenCache,
    steam_by_id: HashMap<u32, SteamGame>,
    /// Wall-clock ms of the last successful library seed/scan (cache
    /// `scanned_at_ms` or live scan time). Shown as "Last synced" in
    /// Settings. `None` before the first success.
    steam_last_synced_ms: Option<u64>,
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
    nav_source_logged: Option<&'static str>,
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
    /// One-shot when launcher_nav_ready stays false after open.
    nav_not_ready_warned: bool,
    /// When the start window was opened (for not-ready timing).
    launcher_opened_at: Option<Instant>,
    /// Last applied launcher selection (restart restores, row rebuilds
    /// re-pin by key while the restore settle window is fresh).
    launcher_selection_shadow: Option<crate::session::LauncherSelection>,
    /// Renderer prefs at boot (AA/vsync are boot-only iced settings).
    renderer_boot_aa: bool,
    renderer_boot_vsync: bool,
    /// True while exiting for a renderer-pref reload (keeps the restore
    /// record; plain Quit clears it so a relaunch starts clean).
    restarting_for_renderer: bool,
    /// Reload button pressed, restart due after a short feedback delay.
    /// Disables the button and spins both reload buttons until exit.
    renderer_reload_queued: bool,
    /// One-shot pad-nav arming variant for the next `open_launcher`.
    /// Reload restore sets all UI state (drawer, scrolls, selections) BEFORE
    /// opening; the restore open takes this path instead of the standard
    /// prepare (see `apply_restart_ui`).
    restore_preserve_nav: bool,
    /// Saved settings-drawer offset to re-apply on `LauncherOpened`, when the
    /// scrollable exists. One-shot per window open and never re-asserted.
    pending_settings_scroll: Option<f32>,
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
    /// Wall-clock deadline for the active toast (`TOAST_LIFETIME` after show).
    /// The `ToastDismiss` delay task already fires on a worker thread, but its
    /// delivery stalls while the iced loop is blocked (e.g. native tray-menu
    /// modal loop). This deadline lets the first `ToastFrame` after resume
    /// dismiss immediately instead of extending the lifetime by the stall.
    toast_expire_at: Option<Instant>,
    /// Pure toast / deferred-Launcher presentation machine.
    toast_machine: toast_machine::State,
    /// True until the first Launcher HWND of this process finishes its cold-load ceremony.
    launcher_cold_load_remaining: bool,

    /// Bumped on every spectrum edit; stale SpectrumCommit messages are ignored.
    spectrum_generation: u64,
    /// GitHub-release update check state (Settings → System → Version).
    update_status: UpdateStatus,
    /// Once-per-version notification record (`updater.json`).
    updater_state: UpdaterState,

    /// Emulated pads the developer emulator knows about, connected or not.
    /// Debug-only; the session sees the connected slice via
    /// [`App::live_controllers`].
    #[cfg(debug_assertions)]
    emulated_pads: Vec<emulate::EmulatedPad>,
    /// Battery levels previewed but not yet published, keyed by serial.
    #[cfg(debug_assertions)]
    pending_emulator_commits: std::collections::HashMap<Serial, PendingEmulatorCommit>,
}

/// A battery level the slider previewed, waiting out its debounce.
///
/// Publishing runs the normal connect/notify path, so it is deliberately
/// debounced: a drag must not emit a toast (or open Launcher) per frame.
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
    // Adapter logging creates a wgpu Instance; it runs at the end of `boot`
    // so CEF initialization and the persistent profile are in place first.

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

        crate::platform::trace::manifest("prefs", &prefs);
        crate::platform::trace::manifest("games", &games);
        crate::platform::trace::manifest("known_controllers", known.remembered_disconnected(&[]));

        #[cfg(windows)]
        Host::ensure_quiet_entry();

        let hid_worker = HidWorkerHandle::start(spectrum.clone());
        let identifying = hid_worker.identifying();
        let input_hub = hid_worker.input_hub();
        let claims = hid_worker.claims();
        claims.set_enabled(prefs.lightbar_enabled);
        let art = Arc::new(ArtOwner::new());
        let sound = SoundHub::new();

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
        // The browser card is height-driven (16:9): resolve the primary
        // monitor before spawning the engine so the software surface is
        // created at its final page size. Starting at a placeholder size
        // would reflow the first document (and leave it laid out at the old
        // width if only the pixel buffer grew).
        let initial_cover =
            crate::ui::layout::primary_monitor_cover().unwrap_or(crate::ui::layout::MonitorCover {
                x: 0.0,
                y: 0.0,
                width: 1920.0,
                height: 1080.0,
            });
        let (initial_page_w, initial_page_h) =
            crate::ui::browser::view::fullscreen_layout(initial_cover.width, initial_cover.height);
        let initial_page = (
            initial_page_w.round().max(1.0) as u32,
            initial_page_h.round().max(1.0) as u32,
        );
        // A crash-restart successor must not re-open the page that killed
        // CEF can crash on a hostile page too. Boot the default page; saved tabs stay
        // stored for a manual switch.
        let crash_restart_boot = crate::platform::crash_restart::has_pending_notice()
            || crate::platform::crash_restart::recent_crash()
            || crate::platform::crash_restart::take_panic_flag();
        let boot_url = if crash_restart_boot {
            crate::browser::BROWSER_DEFAULT_URL.to_string()
        } else {
            crate::browser::tabs::boot_url()
        };
        #[cfg(feature = "cef-engine")]
        let browser_engine = {
            crate::browser::engine_install::cleanup_pending_uninstall();
            crate::browser::cef_backend::spawn_engine(&boot_url, initial_page)
        };
        #[cfg(not(feature = "cef-engine"))]
        let browser_engine: Option<EngineHandle> = {
            // UI-only iteration build: no engine, no reader fallback.
            let _ = (&boot_url, initial_page);
            None
        };
        let install_limit = session.prefs.apps_download_limit_kbps;
        let replay = crate::platform::trace::replay::request()
            .and_then(crate::platform::trace::replay::plan);
        let osk_slot = input_hub.osk_slot();
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
            sound,
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
            debug_browser: false,
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
            launcher_window: None,
            launcher_visible: false,
            shell_window: None,
            shell_mode: home::ShellScreen::Home,
            shell_hidden: true,
            shell_pending_show: false,
            pending_shell_mode: None,
            shell_visibility_epoch: std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0)),
            browser_window: None,
            browser_visible: false,
            browser_monitor_cover: None,
            browser_window_size: None,
            browser_shell_anim: None,
            launcher_shell_anim: None,
            shell_backdrop: crate::ui::motion::ShellBackdrop::Hidden,
            home_window: None,
            home_visible: false,
            ps_resume: None,
            ps_resume_pending: false,
            home_stage: (crate::ui::home::HOME_WIDTH, crate::ui::home::HOME_HEIGHT),
            home_shell_anim: None,
            home_monitor_cover: None,
            home_state: crate::ui::home::State::default(),
            last_start_paint_at: std::cell::Cell::new(None),
            last_start_paint_active: std::cell::Cell::new(false),
            launcher_nav_ready: false,
            launcher_state: launcher_view::State::default(),
            browser: {
                let mut browser = browser::BrowserAppState {
                    engine: browser_engine,
                    ..Default::default()
                };
                browser.dpad_stepper = NavStepper::with_profile(NavProfile::BROWSER_DPAD);
                browser
            },
            osk: osk::SystemOsk::new(osk_slot),
            install_policy: installs::AppInstallPolicy::new(install_limit),
            home_apps: home::HomeApps::new(),
            launcher_monitor_cover: None,
            crash_restart_boot,
            launcher_last_activity: Instant::now(),
            shell_cursor_last_active: Instant::now(),
            launcher_home_pause: None,
            shell_cursor_hidden: false,
            shell_opening: false,
            shell_presented: false,
            shell_reveal_armed: false,
            shell_reveal_generation: 0,
            shell_reveal_started: None,
            launcher_file_dialog_open: false,
            ps_release: ChordReleaseDetector::default(),
            home_face_latch_until: None,
            launcher_revealed_at: None,
            pad_nav: crate::domain::navigation::NavDispatcher::new(),
            pad_actions: PadActionGate::default(),
            pending_pad_actions: VecDeque::new(),
            pending_browser_trace: None,
            pending_history_travel: None,
            replay,
            replay_wait: None,
            replay_wait_seq: 0,
            replay_capture_pending: 0,
            #[cfg(debug_assertions)]
            replay_capture_retries: 0,
            replay_finished: false,
            replay_prime_done: false,
            pad_input_owner: PadInputOwner::None,
            manual_add_r2_latched: false,
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
            seen_cache: SeenCache::new(),
            steam_by_id: HashMap::new(),

            steam_last_synced_ms: None,
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
            launcher_opened_at: None,
            launcher_selection_shadow: None,
            renderer_boot_aa,
            renderer_boot_vsync,
            restarting_for_renderer: false,
            renderer_reload_queued: false,
            restore_preserve_nav: false,
            pending_settings_scroll: None,
            scroll_restore_until: None,
            pending_configure_scroll: None,
            toast_window: None,
            toast_message: None,
            toast_queue: VecDeque::new(),
            toast_generation: 0,
            toast_placement: None,
            toast_anim_started: Instant::now(),
            toast_expire_at: None,
            toast_machine: toast_machine::State::Idle,
            launcher_cold_load_remaining: true,
            spectrum_generation: 0,
            update_status: UpdateStatus::Idle,
            updater_state: UpdaterState::load(),
            #[cfg(debug_assertions)]
            emulated_pads,
            #[cfg(debug_assertions)]
            pending_emulator_commits: std::collections::HashMap::new(),
        };

        // Apply the boot-time browser geometry to the panel/cursor now; the
        // browser window may not open for a while and must be ready at the
        // right fullscreen size when it does.
        app.browser
            .panel
            .set_viewport(initial_page_w.max(1.0), initial_page_h.max(1.0));
        // The default cursor is centered in the compile-time 800x600 box;
        // recenter it in the real boot page area before the first Browser
        // frame so it never appears near the top-left corner.
        app.browser.panel.cursor.reset_for_page();

        // Disk-persisted bookmarks feed the tabs grid star and bookmark row.
        app.browser.panel.bookmarks = crate::browser::bookmarks::load();

        // Show the tray immediately; the daemon owns it for its whole lifetime.
        app.create_tray();
        // Forensics line: which binary, which install flavor, and whether the
        // boot check is armed. Dev log only, so the usual triage questions
        // (version? packaged?) answer themselves.
        crate::platform::updater::diag_update(format!(
            "{} {} starting packaged={} update_boot_check={}",
            crate::platform::app_meta::DISPLAY_NAME,
            crate::platform::app_meta::PKG_VERSION,
            u8::from(crate::platform::packaged::is_packaged()),
            u8::from(app.session.prefs.update_boot_check),
        ));
        #[cfg(all(windows, debug_assertions))]
        start_hitch_hotkey_worker(app.input_hub.clone());
        app.sync_popup_rows();
        app.sync_low_battery();
        app.refresh_analytics_panel();
        app.seed_steam_library_from_cache();
        // Engine presence is process-static: sync once so the browser card
        // can label its backend (engine pixels vs reader text).
        app.browser.engine_available = app.browser.engine.is_some();
        #[cfg(feature = "cef-engine")]
        {
            app.browser.engine_status = if app.browser.engine.is_some() {
                crate::app::browser::EngineStatus::Ready(
                    crate::browser::cef_backend::resolve_engine_dir().unwrap_or_default(),
                )
            } else {
                crate::app::browser::EngineStatus::Missing
            };
        }
        if app.browser.engine.is_some() {
            crate::platform::app_log::info("browser: engine backend active");
        } else {
            crate::platform::app_log::info("browser: reader backend active");
        }

        // Renderer-pref restart: reopen to the pre-restart UI state.
        // Consumed once; stale (>120s) records are ignored by `take()`.
        let restart_restore = crate::persist::restart_ui::take()
            .map(|rs| app.apply_restart_ui(rs))
            .unwrap_or_else(Task::none);

        let toast_boot = app
            .ensure_toast_window()
            .chain(app.maybe_show_crash_restart_toast())
            .chain(app.maybe_show_startup_notice());
        // Live Steam scan kicks on LauncherOpened (or idle fallback when Launcher is disabled).
        let steam_fallback = Task::perform(delay(STEAM_SCAN_IDLE_FALLBACK), |()| {
            Message::SteamScanIdleFallback
        });
        // Automatic release check (opt-out in Settings → System → Version).
        let update_boot = if app.session.prefs.update_boot_check {
            Task::perform(delay(crate::platform::updater::UPDATE_BOOT_DELAY), |()| {
                Message::BootUpdateCheck
            })
        } else {
            Task::none()
        };
        #[cfg_attr(not(debug_assertions), allow(unused_mut))]
        let mut task = Task::batch([
            app.request_refresh(),
            toast_boot,
            steam_fallback,
            restart_restore,
            update_boot,
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
        if let Some(plan) = &app.replay {
            let request = crate::platform::trace::replay::request();
            crate::platform::trace::manifest(
                "replay.start",
                serde_json::json!({
                    "flow": plan.flow(),
                    "session": request.map(|r| &r.session),
                    "sandbox": request.map(|r| &r.sandbox),
                }),
            );
            task = Task::batch([
                task,
                Task::perform(delay(Duration::from_secs(2)), |()| Message::ReplayStep),
            ]);
        }
        // Adapter logging creates a wgpu Instance; run it after the engine
        // is initialized but before iced boots any window.
        crate::platform::wgpu_diag::log_adapters_at_boot();
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
                        "launcher" => Some(Section::Launcher),
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
                "home" => tasks.push(self.open_home()),
                "browser" => tasks.push(self.open_browser_window()),
                "osk" => tasks.push(self.request_osk_open(crate::ui::osk::OskTarget::BrowserPage)),
                "launcher" => tasks.push(self.open_launcher()),
                "launcher-edit" => {
                    tasks.push(self.open_launcher());
                    let _ = self.toggle_launcher_edit();
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
        } else if Some(window) == self.launcher_window
            || Some(window) == self.home_window
            || Some(window) == self.browser_window
            || Some(window) == self.osk.window
        {
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
            iced::event::listen_with(|event, _status, id| {
                // Real input is off for automated/quiet sessions. Replay
                // injects synthetic keys/mouse directly into app handlers.
                if crate::platform::debug_quiet::input_dropped()
                    && matches!(
                        event,
                        iced::Event::Keyboard(_)
                            | iced::Event::Mouse(_)
                            | iced::Event::Touch(_)
                            | iced::Event::InputMethod(_)
                    )
                {
                    return None;
                }
                match event {
                    iced::Event::Window(window::Event::Unfocused) => {
                        Some(Message::WindowUnfocused(id))
                    }
                    iced::Event::Window(window::Event::Focused) => Some(Message::WindowFocused(id)),
                    iced::Event::Keyboard(keyboard::Event::KeyPressed {
                        key,
                        physical_key,
                        location,
                        modifiers,
                        repeat,
                        ..
                    }) => {
                        #[cfg(debug_assertions)]
                        if key == keyboard::Key::Named(keyboard::key::Named::F9) {
                            return Some(Message::ToggleDebugLayoutBounds);
                        }
                        #[cfg(debug_assertions)]
                        if key == keyboard::Key::Named(keyboard::key::Named::F10) {
                            return Some(Message::ToggleDebugBrowser);
                        }
                        Some(Message::BrowserRawKey {
                            id,
                            key: RawKey {
                                key,
                                physical: physical_key,
                                location,
                                modifiers,
                                pressed: true,
                                repeat,
                            },
                        })
                    }
                    iced::Event::Keyboard(keyboard::Event::KeyReleased {
                        key,
                        physical_key,
                        location,
                        modifiers,
                        ..
                    }) => Some(Message::BrowserRawKey {
                        id,
                        key: RawKey {
                            key,
                            physical: physical_key,
                            location,
                            modifiers,
                            pressed: false,
                            repeat: false,
                        },
                    }),
                    iced::Event::Mouse(
                        mouse::Event::CursorMoved { .. }
                        | mouse::Event::ButtonPressed(_)
                        | mouse::Event::ButtonReleased(_)
                        | mouse::Event::WheelScrolled { .. },
                    ) => Some(Message::ShellCursorActivity(id)),
                    iced::Event::Touch(_) => Some(Message::ShellCursorActivity(id)),
                    iced::Event::InputMethod(event) => Some(Message::BrowserIme { id, event }),
                    _ => None,
                }
            }),
        ];

        // Replay settle: wake the driver on the redraw that a step requested.
        // Only alive while a gate actually needs a present, so normal idle
        // sessions never subscribe to frames.
        if self.replay_wants_frame() {
            subscriptions.push(iced::window::frames().map(|_| Message::ReplayFrame));
        }

        subscriptions.push(
            iced::time::every(
                if self.session.prefs.home_enabled && self.session.controllers.is_empty() {
                    PRESENCE_INTERVAL_EMPTY
                } else {
                    PRESENCE_INTERVAL
                },
            )
            .map(|_| Message::Tick),
        );
        subscriptions.push(Subscription::run(tray_events_mapped));
        subscriptions
            .push(window::resize_events().map(|(id, size)| Message::WindowSized(id, size)));
        // OS device arrival/removal wake (event-driven; the Tick timer above
        // is only a backstop). Always subscribed like tray events: the stream
        // owns its watcher registration for the process lifetime.
        subscriptions.push(Subscription::run(presence_hint_stream));

        if self.toast_message.is_some() && !crate::platform::debug_quiet::input_dropped() {
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

        if self.browser_shell_anim.is_some()
            || self.home_shell_anim.is_some()
            || self.launcher_shell_anim.is_some()
            || self.shell_backdrop.animating(Instant::now())
        {
            subscriptions.push(iced::time::every(UI_TICK).map(|_| Message::ShellFadeTick));
        }
        // Only while the offscreen prime is armed: one redraw is all the
        // reveal gate needs, so this never stays subscribed.
        if self.shell_reveal_armed && !self.shell_presented {
            // `window::events` uses the filtered listener, which deliberately
            // drops RedrawRequested. The raw listener is the only way to know
            // the primed frame painted.
            subscriptions.push(iced::event::listen_raw(|event, _status, id| {
                matches!(
                    event,
                    iced::Event::Window(iced::window::Event::RedrawRequested(_))
                )
                .then_some(Message::ShellRedraw(id))
            }));
        }

        if self.toast_message.is_some() && !self.toast_machine.is_resting() {
            // Placing/slide only: a resting card is static and expiry is a
            // one-shot task, so it must not keep a global 60Hz clock alive.
            subscriptions.push(iced::time::every(UI_TICK).map(|_| Message::ToastFrame));
        }

        // The local uninstall is almost instant, but the inline meter still
        // animates while the worker closes and the pointer is dropped.
        if self.home_visible && self.home_state.uninstalling {
            subscriptions.push(iced::time::every(UI_TICK).map(|_| Message::HomeProgressTick));
        }
        if self.home_visible && self.home_state.controller_anim_needs_frames() {
            subscriptions.push(iced::time::every(UI_TICK).map(|_| Message::HomeControllerTick));
        }
        if self.home_visible && self.home_state.hold_animating(Instant::now()) {
            subscriptions.push(iced::time::every(UI_TICK).map(|_| Message::HomeFrame));
        }

        if self.popup_state.identify_flash_active()
            || self.home_state.identify_flash_active()
            || (self.popup_window.is_some() && self.popup_state.entrance_active(Instant::now()))
        {
            subscriptions.push(iced::time::every(UI_TICK).map(|_| Message::IdentifyFrame));
        }

        let pad_input_live =
            self.configure_window.is_some() && self.configure_state.section == Section::PadInput;
        let pad_listening = !crate::platform::debug_quiet::input_dropped()
            && (pad_input_live
                || self.home_visible
                || self.browser_visible
                || self.osk.state.open
                || (self.session.prefs.home_enabled && !self.session.controllers.is_empty()));
        self.hid_worker.set_input_hot(pad_listening);
        self.input_hub.set_osk_listening(
            self.osk.state.open && !crate::platform::debug_quiet::input_dropped(),
        );
        self.input_hub.set_browser_listening(
            self.browser_visible
                && !self.home_visible
                && !self.launcher_visible
                && !self.osk.state.open
                && !crate::platform::debug_quiet::input_dropped(),
        );
        if pad_listening {
            subscriptions.push(Subscription::run_with(
                self.input_hub.clone(),
                pad_input_edge_stream,
            ));
        }

        // CEF engine worker → UI event bridge. The worker stays suspended
        // until the Browser window is visible.
        if let Some(engine) = self.browser.engine.clone() {
            subscriptions.push(Subscription::run_with(engine, engine_event_stream));
        }

        // Home's full-screen overlay owns the screen; freezing Launcher's
        // 60Hz clock avoids advance-then-catch-up judder in the dimmed
        // game art underneath. Frames resume when Home closes.
        if self.launcher_visible
            && !self.home_visible
            && (self.launcher_state.needs_frames() || self.steam_scan_in_flight())
        {
            #[cfg(debug_assertions)]
            if self.launcher_state.needs_frames() && std::env::var_os("SDSC_DEBUG_FRAMES").is_some()
            {
                use std::sync::Mutex;
                static LAST: Mutex<Option<Instant>> = Mutex::new(None);
                let now = Instant::now();
                if let Ok(mut guard) = LAST.lock() {
                    let due =
                        guard.is_none_or(|last| now.duration_since(last) >= Duration::from_secs(1));
                    if due {
                        *guard = Some(now);
                        app_log::info(format!(
                            "ui-frames: launcher reason={}",
                            self.launcher_state.frames_reason()
                        ));
                    }
                }
            }
            subscriptions.push(iced::time::every(UI_TICK).map(|_| Message::LauncherFrame));
        } else if self.launcher_visible && !self.home_visible && !self.osk.state.open {
            // No animation needs frames: keep the wall-clock idle/sleep state
            // moving at 2Hz instead of a 60Hz ambient tick.
            subscriptions.push(
                iced::time::every(Duration::from_millis(500)).map(|_| Message::LauncherIdleTick),
            );
        }
        // One native-cursor clock for the whole shell. It keeps running across
        // Launcher/Home/Browser swaps and is the only thing that hides/shows
        // the pointer while a shell screen is visible.
        if self.home_visible || self.browser_visible || self.launcher_visible {
            subscriptions.push(
                iced::time::every(Duration::from_millis(500)).map(|_| Message::ShellCursorTick),
            );
        }
        // A history jump that never reports a landing/error must still settle.
        // Independent of the cursor surface so an idle cursor cannot strand it.
        if self.pending_history_travel.is_some() {
            subscriptions.push(
                iced::time::every(Duration::from_millis(250))
                    .map(|_| Message::BrowserHistoryTimeout),
            );
        }
        #[cfg(debug_assertions)]
        if self.browser_visible && self.debug_browser {
            subscriptions.push(
                iced::time::every(Duration::from_millis(250))
                    .map(|_| Message::BrowserDebugShotTick),
            );
        }

        if self.popup_window.is_some() || self.configure_window.is_some() {
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

        if self.session.prefs.home_enabled {
            subscriptions.push(
                Subscription::run_with(
                    self.art.clone(),
                    crate::ui::launcher::art_worker::event_stream,
                )
                .map(Message::LauncherArtEvent),
            );
        }

        // Wall-clock owner for the running-game badge: pad edges only flow
        // while Launcher is open and pads are live, so without this a failed
        // immersive launch (Launcher auto-closed) would freeze `Playing` until
        // the next open. Only subscribed while a session is alive.
        if self.running_session.is_some() {
            let interval = if self.launcher_visible {
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
        let content = self.view_inner(window);
        #[cfg(debug_assertions)]
        let content = crate::ui::redraw_probe::probe(window, content);
        // Automated/quiet sessions never accept real keyboard or mouse input;
        // replay injects synthetic events into the same handlers instead.
        if crate::platform::debug_quiet::input_dropped() {
            return crate::ui::input_lock::input_lock(content);
        }
        content
    }

    fn view_inner(&self, window: window::Id) -> Element<'_, Message> {
        #[cfg(debug_assertions)]
        crate::platform::redraw_stats::note_view(window, self.redraw_window_kind(window));
        if Some(window) == self.popup_window {
            return popup_view::view(
                &self.popup_state,
                &self.popup_rows,
                &self.session.prefs.spectrum,
                self.launcher_state.powering_off.as_ref(),
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

        if Some(window) == self.osk.window {
            // Warm window, closed keyboard: keep the view cheap. Rebuilding
            // the full layout on every app update was enough to jank the
            // cursor after the first OSK open/close.
            if !self.osk.state.open {
                return crate::ui::osk::empty();
            }
            return crate::ui::osk::surface(
                &self.osk.state,
                Arc::clone(&self.osk.slot),
                self.osk.nav_epoch,
            )
            .map(Message::Osk);
        }

        if Some(window) == self.browser_window {
            let (w, h) = self
                .browser_window_size
                .or_else(|| self.browser_monitor_cover.map(|c| (c.width, c.height)))
                .unwrap_or((1920.0, 1080.0));
            let now = Instant::now();
            let reduced =
                crate::ui::motion::reduced_motion_effective(self.session.prefs.reduced_motion);
            let panel = self
                .browser_shell_anim
                .map(|anim| anim.frame(now, reduced).panel)
                .unwrap_or_else(crate::ui::home::PanelVisual::full);
            let backdrop_alpha = self.shell_backdrop_alpha(now);
            let shell = crate::ui::home::ShellFrame {
                panel,
                backdrop_alpha,
            };
            let draws = crate::ui::browser::view::cursor_draws(&self.browser.panel)
                && self.browser_shell_anim.is_none();
            let cursor = crate::ui::browser::cursor_surface::CursorSurfaceSpec {
                handle: self.browser.panel.cursor.clone(),
                hub: self.input_hub.clone(),
                engine: self.browser.engine.clone(),
                tab: self.browser.panel.active_tab,
                active: draws
                    && !self.home_visible
                    && !self.launcher_visible
                    && !self.osk.state.open,
                draws,
            };
            return crate::ui::browser::view::root_view(
                &self.browser.panel,
                self.browser.engine_available,
                w,
                h,
                shell,
                cursor,
                self.shell_cursor_hidden || !self.browser.panel.page_mouse_active,
            )
            .map(Message::Browser);
        }

        if Some(window) == self.home_window {
            let now = Instant::now();
            let reduced =
                crate::ui::motion::reduced_motion_effective(self.session.prefs.reduced_motion);
            let shell = self
                .home_shell_anim
                .map(|anim| anim.frame(now, reduced))
                .unwrap_or_else(crate::ui::home::ShellFrame::full);
            let backdrop_alpha = self.shell_backdrop_alpha(now);
            let home_panel = crate::ui::home::view(
                &self.home_state,
                &self.session.prefs.spectrum,
                self.home_stage.0,
                self.home_stage.1,
                backdrop_alpha,
                shell.panel,
                now,
            )
            .map(Message::Home);
            // Launcher hosts the toast card while its cover is up; Home
            // overwrites that window with the scrim, so draw the same card in
            // Home's full-screen window too. The HWND toast is hidden in
            // this mode, so there is no duplicate.
            let home_surface: Element<'_, Message> = if self.home_stage.0
                > crate::ui::home::HOME_WIDTH + 1.0
                && let Some(toast) = self.immersive_toast_overlay()
            {
                stack![home_panel, toast].width(Fill).height(Fill).into()
            } else {
                home_panel
            };
            return cursor_hide::force_cursor(home_surface, self.shell_cursor_hidden).into();
        }

        if Some(window) == self.launcher_window {
            // Paint-gap witness (black-lockup forensics): while the enter
            // ceremony is live the window must repaint ~60Hz. A gap here
            // means paints starved while logic ran ahead — the render/present
            // side, never the state machine. Armed only across live phases,
            // so idle gaps never log.
            let paint_now = Instant::now();
            let paint_active = self.launcher_state.transition_phase.is_some();
            if paint_active
                && self.last_start_paint_active.get()
                && let Some(last) = self.last_start_paint_at.get()
            {
                let gap_ms = paint_now.saturating_duration_since(last).as_millis() as u64;
                if gap_ms > 250 {
                    let phase = self
                        .launcher_state
                        .transition_phase
                        .map(|(p, _)| p.label())
                        .unwrap_or("none");
                    crate::controller::hid::diag::diag_info(format!(
                        "ui-diag: launcher paint gap gap_ms={gap_ms} phase={phase}"
                    ));
                }
            }
            self.last_start_paint_at.set(Some(paint_now));
            self.last_start_paint_active.set(paint_active);
            let cover = self.launcher_monitor_cover.map(|c| (c.width, c.height));
            let stage_w = cover.map(|(w, _)| w).unwrap_or(1920.0);
            let stage_h = cover.map(|(_, h)| h).unwrap_or(1080.0);
            let settings_snapshot = self.launcher_settings_snapshot();
            // Ambient/ken-burns freeze while Home covers the Launcher and
            // under the immersive performance throttle when no real animation
            // needs frames (the old bare-idle gate semantics).
            let ambient_paused = !self.launcher_visible
                || self.home_visible
                || (self.session.prefs.perf_throttle_immersive
                    && !self.launcher_state.needs_frames());
            let start = launcher_view::view(
                &self.launcher_state,
                Instant::now(),
                self.session.prefs.launcher_clock_enabled,
                stage_h,
                stage_w,
                &settings_snapshot,
                ambient_paused,
                #[cfg(debug_assertions)]
                self.debug_layout_bounds,
            )
            .map(Message::Launcher);
            // Debug R3 hide shows pure ambience: no toast composite either.
            #[cfg(debug_assertions)]
            let start = if self.launcher_state.ambient_tuning_active()
                && self.launcher_state.ambient_tune_hide_chrome
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
            // Shared shell transition: the Launcher is full-bleed. Never
            // scale or resize its view; only fade a black veil using the same
            // panel alpha Home and Browser animate.
            let start: Element<'_, Message> = if let Some(anim) = self.launcher_shell_anim {
                let now = Instant::now();
                let reduced =
                    crate::ui::motion::reduced_motion_effective(self.session.prefs.reduced_motion);
                let frame = anim.frame(now, reduced);
                let veil = 1.0 - frame.panel.alpha.clamp(0.0, 1.0);
                if veil > 0.001 {
                    let scrim = container(space())
                        .width(Fill)
                        .height(Fill)
                        .style(crate::ui::theme::scrim(veil));
                    stack![start, scrim].width(Fill).height(Fill).into()
                } else {
                    start
                }
            } else {
                start
            };
            return cursor_hide::force_cursor(start, self.shell_cursor_hidden).into();
        }

        if Some(window) == self.toast_window {
            // Immersive Launcher composites the card; keep the toast HWND blank.
            if self.immersive_hosts_toast() {
                return toast_view::empty();
            }
            return match self.toast_message.as_ref() {
                Some(message) => toast_view::view(
                    message,
                    self.toast_generation,
                    Message::ToastDismiss(self.toast_generation),
                    self.toast_opacity(),
                    self.toast_slide_offset(),
                ),
                None => toast_view::empty(),
            };
        }

        container(space()).style(theme::root).into()
    }

    fn update(&mut self, message: Message) -> Task<Message> {
        if self.renderer_reload_queued && message.is_user_input() {
            if let Some(trace) = message.trace_action() {
                trace
                    .begin()
                    .finish("suppressed")
                    .field("reason", "renderer_reload")
                    .emit();
            }
            return Task::none();
        }
        let is_replay_step = matches!(&message, Message::ReplayStep);
        let trace = message.trace_action();
        let task = self.update_inner(message);
        if let Some(trace) = trace {
            trace.begin().finish("dispatched").emit();
        }
        if is_replay_step {
            task
        } else {
            task.chain(self.replay_kick())
        }
    }

    fn update_inner(&mut self, message: Message) -> Task<Message> {
        #[cfg(debug_assertions)]
        crate::platform::redraw_stats::note_message(redraw_message_source(&message));
        // Effective reduce-motion gate (prefs choice vs OS "Show animations"
        // toggle): synced on every update so pad/keyboard/mouse-initiated
        // anims gate instantly, even with an idle Launcher (no LauncherFrame) or
        // prefs freshly arrived via snapshot. Re-syncing while set is a
        // no-op (chases are already settled); LauncherFrame re-syncs too so an
        // OS-toggle flip lands mid-session.
        let reduced =
            crate::ui::motion::reduced_motion_effective(self.session.prefs.reduced_motion);
        self.launcher_state.sync_reduced_motion(reduced);
        self.home_state.reduced_motion = reduced;
        self.launcher_state
            .sync_perf_throttle(self.session.prefs.perf_throttle_immersive);
        // Debug tuning keep-alive follows the local fields (the tuning
        // arms below re-sync too, since this runs before the match).
        #[cfg(debug_assertions)]
        {
            self.launcher_state.ambient_tuning_keepalive = self.debug_tuning_active();
        }
        match message {
            Message::Tick => self.on_tick(),
            Message::ReplayStep => self.on_replay_step(),
            Message::ReplayFrame => {
                self.replay_note_frame();
                Task::none()
            }
            Message::ReplayWaitTimeout(id) => self.on_replay_wait_timeout(id),
            Message::ReplayWaitGrace(id) => self.on_replay_wait_grace(id),
            Message::ReplayCaptureDone => self.on_replay_capture_done(),
            Message::PresenceHint => self.on_presence_hint(),
            Message::AbsenceRecheck => self.on_absence_recheck(),
            #[cfg(debug_assertions)]
            Message::ToggleDebugLayoutBounds => {
                self.debug_layout_bounds = !self.debug_layout_bounds;
                Task::none()
            }
            #[cfg(debug_assertions)]
            Message::ToggleDebugBrowser => {
                self.debug_browser = !self.debug_browser;
                self.browser.panel.debug_overlay = self.debug_browser;
                Task::none()
            }
            #[cfg(debug_assertions)]
            Message::EmulatorCommitTick => self.on_emulator_commit_tick(),
            Message::PollResult(result) => self.on_poll_result(result),
            Message::BootUpdateCheck => {
                // A manual or open-triggered check already ran (or is
                // running): the boot check stands down, no second check.
                if matches!(self.update_status, UpdateStatus::Idle) {
                    self.start_update_check("boot")
                } else {
                    crate::platform::updater::diag_update(
                        "update check (boot): skipped, already checked",
                    );
                    Task::none()
                }
            }
            Message::UpdateCheckDone(result) => self.on_update_check_done(result),
            Message::UpdateNotifyDone(_) => self.on_update_notify_done(),
            Message::ProcessEnumResult(result) => self.on_process_enum_result(result),
            Message::SteamAppsChanged => self.on_steam_apps_changed(),
            Message::RunningBadgeTick => self.refresh_running_badge(),
            Message::CloseGameDone(result) => self.on_close_game_done(result),

            Message::TrayMenu(id) => {
                crate::controller::hid::diag::diag_info(format!("ui-diag: tray menu {id}"));
                match id.as_str() {
                    SETTINGS_ID => {
                        // Tray engaged while a shell screen is up: the press never
                        // reaches it (taskbar sits above the cover), so unhide here
                        // or the native menu opens under a hidden cursor.
                        self.note_shell_cursor_activity();
                        self.open_configure()
                    }
                    QUIT_ID => self.update(Message::Exit),
                    _ => Task::none(),
                }
            }
            Message::TrayLeftClick(anchor) => {
                crate::controller::hid::diag::diag_info("ui-diag: tray left-click");
                self.tray_anchor = anchor;
                self.note_shell_cursor_activity();
                self.toggle_popup()
            }

            Message::WindowClosed(id) => self.on_window_closed(id),
            Message::WindowUnfocused(id) => self.on_window_unfocused(id),
            Message::WindowFocused(id) => self.on_window_focused(id),

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
                task.chain(self.raise_toast_above_all())
            }
            Message::Configure(message) => self.on_configure_message(message),
            Message::Browser(message) => self.on_browser_message(message),
            Message::BrowserHistoryTimeout => {
                self.check_history_travel_timeout();
                Task::none()
            }
            #[cfg(debug_assertions)]
            Message::BrowserDebugShotTick => self.shoot_browser_window(false),
            Message::OskOpened(id) => self.on_osk_opened(id),
            Message::Osk(message) => self.on_osk_message(message),
            #[cfg(debug_assertions)]
            Message::OskCapture => self.capture_osk_window(),
            #[cfg(debug_assertions)]
            Message::OskShot(shot) => self.save_osk_shot(shot),
            Message::EngineInstallDone {
                generation,
                outcome,
            } => self.on_engine_install_done(generation, outcome),
            Message::EngineInstallCancel => self.cancel_engine_install(),
            #[cfg(feature = "cef-engine")]
            Message::EngineInstallProgress {
                generation,
                progress,
            } => self.on_engine_install_progress(generation, progress),
            #[cfg(feature = "cef-engine")]
            Message::EngineManifestReady { generation, result } => {
                self.on_engine_manifest_ready(generation, result)
            }
            Message::EngineInfoSize { generation, result } => {
                if generation == self.browser.engine_info_generation
                    && let Ok((size, pending)) = result
                {
                    self.home_state.engine_size = (size > 0).then_some(size);
                    self.home_state.engine_pending_size = pending;
                }
                Task::none()
            }
            #[cfg(feature = "cef-engine")]
            Message::EngineUninstallDone { generation, result } => {
                self.on_engine_uninstall_done(generation, result)
            }
            Message::HomeProgressTick => {
                self.home_state.advance_progress_anim();
                Task::none()
            }
            Message::HomeControllerTick => {
                self.home_state.tick_controller_anim(Instant::now());
                Task::none()
            }
            Message::HomeFrame => self.on_home_frame(),
            Message::WindowSized(id, size) => {
                if self.browser_window == Some(id) {
                    self.on_browser_window_sized(size)
                } else if self.home_window == Some(id) {
                    self.on_home_window_sized(size)
                } else {
                    Task::none()
                }
            }
            Message::WindowShowNow(id) => self.on_shell_show_now(id),
            Message::ShellOpened(id) => self.on_shell_opened(id),
            Message::ShellPresent(id, generation) => {
                if generation == self.shell_reveal_generation {
                    self.on_shell_present(id, "fallback")
                } else {
                    Task::none()
                }
            }
            Message::ShellRevealArmed(id, generation) => {
                if generation == self.shell_reveal_generation {
                    self.on_shell_reveal_armed(id)
                } else {
                    Task::none()
                }
            }
            Message::ShellRedraw(id) => self.on_shell_redraw(id),
            Message::ShellFocusFallback(id, generation) => {
                if generation == self.shell_reveal_generation {
                    self.on_shell_focus_fallback(id)
                } else {
                    Task::none()
                }
            }
            Message::ShellFadeTick => {
                let now = Instant::now();
                let mut redraw = Task::none();
                let mut finish_browser = false;
                if let Some(anim) = self.browser_shell_anim {
                    if let Some(id) = self.browser_window {
                        redraw = redraw.chain(crate::ui::layout::invalidate_window(id));
                    }
                    if anim.done(now) {
                        match anim.dir {
                            crate::ui::home::ShellDir::Enter => {
                                self.browser_shell_anim = None;
                            }
                            crate::ui::home::ShellDir::Exit => finish_browser = true,
                        }
                    }
                }
                if let Some(anim) = self.launcher_shell_anim {
                    if let Some(id) = self.launcher_window {
                        redraw = redraw.chain(crate::ui::layout::invalidate_window(id));
                    }
                    if anim.done(now) {
                        match anim.dir {
                            crate::ui::home::ShellDir::Enter => self.launcher_shell_anim = None,
                            crate::ui::home::ShellDir::Exit => {
                                redraw = redraw.chain(self.finish_launcher_close());
                            }
                        }
                    }
                }
                if let Some(anim) = self.home_shell_anim {
                    if let Some(id) = self.home_window {
                        redraw = redraw.chain(crate::ui::layout::invalidate_window(id));
                    }
                    if anim.done(now) {
                        match anim.dir {
                            crate::ui::home::ShellDir::Enter => self.home_shell_anim = None,
                            crate::ui::home::ShellDir::Exit => {
                                redraw = redraw.chain(self.finish_home_close(now));
                            }
                        }
                    }
                }
                if self.shell_backdrop.animating(now) {
                    if let Some(id) = self.shell_backdrop_window() {
                        redraw = redraw.chain(crate::ui::layout::invalidate_window(id));
                    }
                    if self.shell_backdrop.done(now) {
                        self.shell_backdrop = match self.shell_backdrop {
                            crate::ui::motion::ShellBackdrop::Entering { .. } => {
                                crate::ui::motion::ShellBackdrop::Full
                            }
                            crate::ui::motion::ShellBackdrop::Exiting { .. } => {
                                crate::ui::motion::ShellBackdrop::Hidden
                            }
                            other => other,
                        };
                    }
                }
                if finish_browser {
                    redraw = redraw.chain(self.finish_browser_window_close());
                }
                redraw
            }
            Message::Home(message) => self.on_home_message(message),

            #[cfg(debug_assertions)]
            Message::AmbientPreviewOpened(id) => {
                focus_window(id).chain(self.raise_toast_above_all())
            }
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

            Message::LauncherOpened(id) => self.on_start_opened(id),
            Message::LauncherArtEvent(ev) => self.on_start_art_event(ev),
            Message::LauncherFrame => self.on_start_frame(),
            Message::LauncherIdleTick => {
                let now = Instant::now();
                if self.tick_start_idle(now) {
                    self.pad_nav.prepare_on_open(false);
                    crate::controller::hid::diag::diag_info(
                        "ui-diag: launcher idle pad re-arm after sleep wake",
                    );
                }
                Task::none()
            }
            Message::ShellCursorTick => {
                self.tick_shell_cursor_idle(Instant::now());
                Task::none()
            }
            Message::ShellCursorActivity(id) => {
                if !crate::platform::debug_quiet::input_dropped() {
                    let close = self.close_osk("mouse");
                    let on_shell = (Some(id) == self.launcher_window && self.launcher_visible)
                        || (Some(id) == self.browser_window && self.browser_visible)
                        || (Some(id) == self.home_window && self.home_visible);
                    if on_shell {
                        self.note_shell_cursor_activity();
                    }
                    if Some(id) == self.browser_window && self.browser_visible {
                        // Any real pointer activity on the Browser window wins
                        // over the pad ring, including header hovers.
                        self.browser.panel.page_mouse_active = true;
                    }
                    return close;
                }
                Task::none()
            }
            Message::Launcher(message) => {
                // Mouse / iced UI messages are not pad-originated.
                self.haptic_pad_serial = None;
                self.note_launcher_activity();
                self.on_launcher_message(message)
            }
            Message::BrowserEngine(event) => self.on_engine_event(event),
            Message::LauncherKey {
                id,
                action,
                pressed,
            } => {
                if crate::platform::debug_quiet::input_dropped() {
                    Task::none()
                } else if self.osk.state.open {
                    let close = self.close_osk("keyboard");
                    if action == LauncherKeyAction::Cancel {
                        close
                    } else {
                        close.chain(self.on_start_key(id, action, pressed))
                    }
                } else {
                    self.on_start_key(id, action, pressed)
                }
            }
            Message::BrowserRawKey { id, key } => {
                if crate::platform::debug_quiet::input_dropped() {
                    Task::none()
                } else if self.osk.state.open {
                    self.on_osk_key(id, key)
                } else {
                    self.on_browser_raw_key(id, key)
                }
            }
            Message::BrowserIme { id, event } => {
                if crate::platform::debug_quiet::input_dropped() {
                    Task::none()
                } else {
                    let close = self.close_osk("ime");
                    close.chain(self.on_browser_ime(id, event))
                }
            }
            Message::PadInput(edge) => {
                if crate::platform::debug_quiet::input_dropped() {
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
                // Only when Launcher is off — otherwise LauncherOpened owns the kick
                // so Scanning/Preparing stay visible on first open.
                if self.session.prefs.home_enabled {
                    Task::none()
                } else {
                    self.maybe_kick_deferred_steam_scan()
                }
            }
            // Debug-only renderer stress driver: 5 fast cycles of toast +
            // cold Launcher + configure + edit churn (the atlas-crash shape),
            // then a final open. Takes over windows; see the variant docs.
            #[cfg(debug_assertions)]
            Message::StressWindows(step) => self.on_stress_windows(step),

            #[cfg(debug_assertions)]
            Message::DebugOpen(spec) => self.debug_open(&spec),

            Message::PlaceToast { id, generation } => self.handle_place_toast(id, generation),
            Message::ToastShown { generation } => {
                self.toast_step(toast_machine::Event::Shown { generation })
            }
            Message::ToastFrame => self.handle_toast_frame(),
            Message::IdentifyFrame => {
                self.popup_state.tick_identify_flash();
                self.home_state.tick_identify_flash();
                Task::none()
            }
            // Redraw only: the wall-clock spinner phase advances in `view`.
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

            Message::Exit => self.on_exit(),
        }
    }

    // -----------------------------------------------------------------------
    // update() arms — one focused handler per message group.
    //
    // Behavior-preserving split of the former inline match arms; each fn
    // returns `Task<Message>` exactly like its arm did. No logic change.
    // -----------------------------------------------------------------------

    /// Backstop for OS-initiated closes: once no shell window remains, drop the
    /// opening flag and restore the pointer. A visible replacement screen keeps
    /// the cursor state untouched so swaps stay hidden.
    fn restore_cursor_if_desktop(&mut self) {
        if self.home_window.is_none()
            && self.browser_window.is_none()
            && self.launcher_window.is_none()
            && !self.home_visible
            && !self.browser_visible
            && !self.launcher_visible
        {
            self.shell_opening = false;
            self.reset_shell_cursor_clock();
            self.set_shell_cursor_hidden(false);
        }
    }

    fn on_window_closed(&mut self, id: window::Id) -> Task<Message> {
        if Some(id) == self.popup_window {
            self.popup_window = None;
            self.popup_state.cancel();
            self.sync_toast_zorder()
        } else if Some(id) == self.configure_window {
            self.configure_window = None;
            self.sync_toast_zorder()
        } else if Some(id) == self.shell_window {
            let was_browser = self.browser_visible || self.browser_window.is_some();
            let was_launcher = self.launcher_visible;
            let resume_ps = self.ps_resume_pending;
            self.shell_window = None;
            self.shell_hidden = true;
            self.shell_pending_show = false;
            self.shell_reveal_armed = false;
            self.shell_reveal_started = None;
            self.pending_shell_mode = None;
            self.clear_shell_aliases();
            self.home_shell_anim = None;
            self.browser_shell_anim = None;
            self.launcher_shell_anim = None;
            self.browser_monitor_cover = None;
            self.home_monitor_cover = None;
            self.launcher_monitor_cover = None;
            self.shell_backdrop = crate::ui::motion::ShellBackdrop::Hidden;
            if was_browser && self.browser.panel.session.is_open() {
                let _ = self.close_browser_surface();
            }
            let rehost_toast = self.rehost_toast_after_composite();
            if was_launcher {
                self.pad_nav.reset();
                self.browser.dpad_stepper.reset();
                self.clear_start_nav_diag();
                self.ps_release.reset();
            }
            self.restore_cursor_if_desktop();
            let sync = rehost_toast.chain(self.sync_toast_zorder());
            if resume_ps {
                return sync.chain(self.maybe_resume_ps_after_close());
            }
            if self.session.auto_open_pending
                && self.session.prefs.home_enabled
                && self.session.prefs.home_auto_open.auto_opens()
                && !self.browser_visible
                && crate::session::has_home_presence(
                    &self.session.controllers,
                    self.session.prefs.home_auto_open.includes_usb(),
                )
            {
                crate::controller::hid::diag::diag_info("ui-diag: retry home open after close");
                sync.chain(self.open_home())
            } else {
                sync
            }
        } else if Some(id) == self.osk.window {
            self.on_osk_window_closed(id)
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

    fn on_window_unfocused(&mut self, id: window::Id) -> Task<Message> {
        // The popup is a transient tray flyout: dismiss it when focus moves away.
        if Some(id) == self.popup_window && !self.popup_state.is_editing_any() {
            self.close_popup()
        } else if Some(id) == self.launcher_window && self.launcher_visible {
            // Dismiss-on-unfocus stays off until the settings screen ships
            // a proper toggle for it (close-on-focus-loss vs stay-open).
            // Until then Launcher only closes on explicit dismiss, so tray
            // quit can happen with the browser surface still open.
            const CLOSE_ON_UNFOCUS: bool = false;
            if !CLOSE_ON_UNFOCUS {
                return Task::none();
            }
            // File dialogs (add/edit shortcut, choose image) steal focus — keep the
            // launcher open so the modal / draft is not discarded.
            if self.launcher_state.manual_add.is_some() || self.launcher_file_dialog_open {
                Task::none()
            } else if self
                .launcher_revealed_at
                .is_some_and(|t| t.elapsed() < LAUNCHER_UNFOCUS_GRACE)
            {
                crate::controller::hid::diag::diag_info(
                    "ui-diag: launcher unfocus skipped (reveal grace)",
                );
                Task::none()
            } else {
                self.close_launcher()
            }
        } else if Some(id) == self.browser_window && self.browser_visible {
            // A drag/selection must not survive alt-tab; clear held buttons
            // and stale Shift/Ctrl state.
            self.browser_pointer_focus_lost();
            Task::none()
        } else {
            Task::none()
        }
    }

    fn on_start_opened(&mut self, id: window::Id) -> Task<Message> {
        if Some(id) == self.launcher_window && self.launcher_visible {
            self.shell_opening = false;
            self.launcher_nav_ready = true;
            self.nav_not_ready_warned = false;
            let now = Instant::now();
            self.launcher_revealed_at = Some(now);
            self.launcher_state.steam_scan_pending = self.steam_scan_pending;
            crate::controller::hid::diag::diag_info(format!(
                "ui-diag: launcher opened id={id:?} nav_ready={}",
                self.launcher_nav_ready
            ));
            // Raise/focus after create_renderer has a chance to finish —
            // chaining sync on the same turn as window::open correlated
            // with atlas create_renderer panics.
            // Launcher stays normal-level; focus brings it forward without
            // joining the topmost band.
            let mut task = focus_window(id).chain(self.sync_toast_zorder());
            // Settings restore first: the drawer scroll lands
            // before art kicks and scan below.
            if let Some(settings_y) = self.pending_settings_scroll.take() {
                task = task.chain(self.restore_settings_scroll(settings_y));
            } else if self.launcher_state.settings.open {
                // Warm reopen under an open settings drawer: the fresh
                // scrollable mounts at 0, so re-apply the tracked offset
                // (closes preserve it now; restarts use the pending path).
                let settings_y = self.launcher_state.settings.scroll_y;
                task = task.chain(self.restore_settings_scroll(settings_y));
            }
            // Re-show the position rail on every open.
            // Warm opens skip begin_cold_load; cold opens refresh
            // the hide clock to visible time. Pinned is a no-op.
            self.launcher_state.note_position_bar_scroll(now);
            self.launcher_state.arm_cold_ambient(now);
            // Cache miss: need metadata before art/list can land — scan now.
            // Cache hit: Preparing chip first; Steam kicks when art-ready handoff runs.
            if self.steam_scan_pending {
                task = task.chain(self.maybe_kick_deferred_steam_scan());
            } else if let Some(resync) = self.steam_library_resync_task() {
                // Warm reopen: install/uninstall while closed (or a missed
                // watcher event) would otherwise stay stale — the deferred
                // boot scan runs only once per process.
                task = task.chain(resync);
            }
            // Cold: lift the enter veil onto ambient+dock immediately (games list
            // stays hidden until art). Warm reopen still waits for art before arming.
            if self.launcher_state.cold_load_active {
                self.launcher_state.arm_enter_reveal(now);
                // Cache hit: Preparing chip until art sync; cache miss uses Steam.
                if !self.steam_scan_pending {
                    self.launcher_state
                        .begin_chrome_status(launcher_view::ChromeStatusKind::PreparingArt, now);
                }
            }
            self.launcher_state.prime_backdrop_for_enter(now);
            if !self.launcher_state.try_arm_enter_reveal_when_art_ready(now) {
                let open_ms = self
                    .launcher_opened_at
                    .map(|t| now.saturating_duration_since(t).as_millis())
                    .unwrap_or(0);
                crate::controller::hid::diag::diag_info(format!(
                    "ui-diag: launcher immersive enter art hold open_ms={open_ms} steam_scan={} cold={}",
                    u8::from(self.steam_scan_pending),
                    u8::from(self.launcher_state.cold_load_active),
                ));
            } else {
                task = task.chain(self.note_cold_load_finished());
            }
            task = task.chain(self.warm_immersive_art_task());
            task
        } else {
            Task::none()
        }
    }

    fn on_start_art_event(
        &mut self,
        ev: crate::ui::launcher::art_worker::ArtEvent,
    ) -> Task<Message> {
        use crate::ui::launcher::art_worker::ArtEventKind;
        if !crate::ui::launcher::icon_cache::art_warm_is_current(ev.generation) {
            crate::controller::hid::diag::diag_info(format!(
                "ui-diag: art event stale gen={} kind={:?}",
                ev.generation, ev.kind
            ));
            return Task::none();
        }
        let now = Instant::now();
        let armed = match ev.kind {
            ArtEventKind::SelectedReady => {
                if self
                    .launcher_state
                    .rows
                    .get(self.launcher_state.game_selected)
                    .is_some_and(|r| r.backdrop_icon().is_some())
                {
                    self.launcher_state.note_backdrop_ready(now);
                }
                let armed = self.launcher_state.try_arm_enter_reveal_when_art_ready(now);
                crate::controller::hid::diag::diag_info(format!(
                    "ui-diag: immersive art selected ready gen={}",
                    ev.generation
                ));
                armed
            }
            ArtEventKind::NearReady => {
                let armed = self.launcher_state.try_arm_enter_reveal_when_art_ready(now);
                crate::controller::hid::diag::diag_info(format!(
                    "ui-diag: immersive art neighbors ready gen={}",
                    ev.generation
                ));
                armed
            }
            ArtEventKind::JobDone => {
                self.launcher_state.art_awaiting_paint = false;
                let armed = self.launcher_state.try_arm_enter_reveal_when_art_ready(now);
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
        // List / hero / backdrop peeks are live — LauncherFrame redraws pick them up.
        Task::none()
    }

    fn on_start_frame(&mut self) -> Task<Message> {
        // A frame queued before Home opened must not advance/redraw the
        // frozen Launcher animation.
        if self.home_visible {
            return Task::none();
        }
        let now = Instant::now();
        self.launcher_state.steam_scan_pending = self.steam_scan_pending;
        // Prefs-owned pin for the immersive position rail (heals across
        // snapshot syncs / restarts; the apply path syncs immediately).
        self.launcher_state.sync_navbar_pinned(
            matches!(
                self.session.prefs.launcher_navbar,
                NavbarVisibility::AlwaysOn
            ),
            now,
        );
        // Effective reduce-motion gate (also synced per-update): an OS
        // "Show animations" flip lands on the next frame mid-session.
        self.launcher_state
            .sync_reduced_motion(crate::ui::motion::reduced_motion_effective(
                self.session.prefs.reduced_motion,
            ));
        self.launcher_state
            .sync_perf_throttle(self.session.prefs.perf_throttle_immersive);
        let was_cold = self.launcher_state.cold_load_active;
        self.launcher_state.tick_anim(now);
        // Holds are wall-clock state: keep the displayed ring decaying after
        // the pad edge stream quietens (the release tail ends long before the
        // 1.5s decay). Only pull while a ring is already visible so an
        // explicit mode reset in the pad handler is never resurrected.
        if (self.launcher_state.cross_progress > 0.0 || self.launcher_state.triangle_progress > 0.0)
            && self.pad_nav.has_hold_activity()
        {
            let (cross, triangle) = self.pad_nav.hold_progress(now);
            self.launcher_state.cross_progress = cross;
            self.launcher_state.triangle_progress = triangle;
        }
        let task = if was_cold && !self.launcher_state.cold_load_active {
            self.note_cold_load_finished()
        } else {
            Task::none()
        };
        if let Some(phase_task) = self.tick_enter_phase(now) {
            return task.chain(phase_task);
        }
        // Keyboard-only hold progress when pad poll is not running.
        if self.launcher_state.replace_confirm.is_some() {
            let (progress, completed) = self.keyboard_cross_hold.update(self.confirm_key_held, now);
            self.launcher_state.cross_progress = self.launcher_state.cross_progress.max(progress);
            if completed {
                self.haptic_pad_serial = None;
                self.play_ui_cue(UiSoundKind::Hold);
                return task.chain(self.complete_replace_confirm());
            }
        }
        self.sync_launcher_held();
        self.launcher_state.tick_hint_anims(now);
        if self.tick_start_idle(now) {
            self.pad_nav.prepare_on_open(false);
            crate::controller::hid::diag::diag_info(
                "ui-diag: launcher idle pad re-arm after sleep wake",
            );
        }
        task
    }

    fn on_start_key(
        &mut self,
        id: window::Id,
        action: LauncherKeyAction,
        pressed: bool,
    ) -> Task<Message> {
        // Keyboard is never pad-originated.
        self.haptic_pad_serial = None;
        if self.home_visible {
            return Task::none();
        }
        if Some(id) == self.launcher_window && self.launcher_visible {
            self.note_launcher_activity();
            // Cold ambient: only Cancel/Close is live.
            if self.launcher_state.cold_load_active {
                return match action {
                    LauncherKeyAction::Cancel if !pressed => {
                        self.cancel_launcher_holds();
                        self.on_launcher_message(LauncherMessage::Close)
                    }
                    LauncherKeyAction::Cancel => {
                        self.cancel_key_held = pressed;
                        self.sync_launcher_held();
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
                    LauncherKeyAction::Up
                        | LauncherKeyAction::Down
                        | LauncherKeyAction::Left
                        | LauncherKeyAction::Right
                )
                && self.launcher_state.layout_transition_active()
            {
                return Task::none();
            }
            return match action {
                LauncherKeyAction::Up if pressed => {
                    self.cancel_launcher_holds();
                    if self.launcher_state.ambient_tuning_active() {
                        self.on_ambient_tuning_nav(Some(NavAction::Up))
                    } else if self.launcher_state.settings.open {
                        self.on_launcher_settings_nav(Some(NavAction::Up))
                    } else if self.browser.panel.session.is_open() {
                        self.scroll_browser(-crate::ui::browser::view::BROWSER_SCROLL_STEP)
                    } else if self.immersive_horizontal_games_nav() {
                        // Horizontal Games strip scrolls Left/Right only.
                        Task::none()
                    } else {
                        self.on_launcher_message(LauncherMessage::MoveUp)
                    }
                }
                LauncherKeyAction::Down if pressed => {
                    self.cancel_launcher_holds();
                    if self.launcher_state.ambient_tuning_active() {
                        self.on_ambient_tuning_nav(Some(NavAction::Down))
                    } else if self.launcher_state.settings.open {
                        self.on_launcher_settings_nav(Some(NavAction::Down))
                    } else if self.browser.panel.session.is_open() {
                        self.scroll_browser(crate::ui::browser::view::BROWSER_SCROLL_STEP)
                    } else if self.immersive_horizontal_games_nav() {
                        // Horizontal Games strip scrolls Left/Right only.
                        Task::none()
                    } else {
                        self.on_launcher_message(LauncherMessage::MoveDown)
                    }
                }
                LauncherKeyAction::Left if pressed => {
                    self.cancel_launcher_holds();
                    if self.launcher_state.ambient_tuning_active() {
                        self.on_ambient_tuning_nav(Some(NavAction::Left))
                    } else if self.launcher_state.settings.open {
                        self.on_launcher_settings_nav(Some(NavAction::Left))
                    } else if self.immersive_horizontal_games_nav() {
                        self.on_launcher_message(LauncherMessage::MoveUp)
                    } else {
                        Task::none()
                    }
                }
                LauncherKeyAction::Right if pressed => {
                    self.cancel_launcher_holds();
                    if self.launcher_state.ambient_tuning_active() {
                        self.on_ambient_tuning_nav(Some(NavAction::Right))
                    } else if self.launcher_state.settings.open {
                        self.on_launcher_settings_nav(Some(NavAction::Right))
                    } else if self.immersive_horizontal_games_nav() {
                        self.on_launcher_message(LauncherMessage::MoveDown)
                    } else {
                        Task::none()
                    }
                }
                LauncherKeyAction::Confirm => {
                    self.confirm_key_held = pressed;
                    self.sync_launcher_held();
                    if self.launcher_state.replace_confirm.is_some() {
                        if pressed {
                            self.pad_nav.cancel_holds();
                        }
                        Task::none()
                    } else if !pressed {
                        self.cancel_launcher_holds();
                        if self.launcher_state.ambient_tuning_active() {
                            self.on_ambient_tuning_nav(Some(NavAction::Confirm))
                        } else if self.launcher_state.settings.open {
                            self.on_launcher_settings_nav(Some(NavAction::Confirm))
                        } else {
                            self.on_launcher_message(LauncherMessage::Confirm)
                        }
                    } else {
                        Task::none()
                    }
                }
                LauncherKeyAction::Cancel => {
                    self.cancel_key_held = pressed;
                    self.sync_launcher_held();
                    if !pressed {
                        self.cancel_launcher_holds();
                        if self.launcher_state.ambient_tuning_active() {
                            self.on_ambient_tuning_nav(Some(NavAction::Cancel))
                        } else if self.launcher_state.settings.any_menu_open() {
                            // Expanded dropdown owns Cancel: Esc closes the
                            // menu only, exactly like pad Circle — not the
                            // whole settings surface.
                            self.on_launcher_settings_nav(Some(NavAction::Cancel))
                        } else {
                            self.on_launcher_message(LauncherMessage::Close)
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

    #[cfg(debug_assertions)]
    fn on_stress_windows(&mut self, step: u8) -> Task<Message> {
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
            return self.show_next_toast().chain(self.open_launcher());
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
                    .chain(self.open_launcher())
                    .chain(Task::perform(delay(Duration::from_secs(2)), move |()| next))
            }
            // Second cold renderer while toast + Launcher are live.
            1 => self.open_configure().chain(Task::perform(
                delay(Duration::from_millis(1500)),
                move |()| next,
            )),
            2..=4 => self
                .toggle_launcher_edit()
                .unwrap_or_else(Task::none)
                .chain(Task::perform(delay(Duration::from_secs(1)), move |()| next)),
            // Tear it all down; toast expiry overlaps the next cold open.
            _ => {
                let close_cfg = self
                    .configure_window
                    .map(window::close)
                    .unwrap_or_else(Task::none);
                let _ = self.toggle_launcher_edit();
                close_cfg
                    .chain(self.close_launcher())
                    .chain(Task::perform(delay(Duration::from_secs(2)), move |()| next))
            }
        }
    }

    fn on_exit(&mut self) -> Task<Message> {
        crate::controller::hid::diag::diag_info("ui-diag: process exit begin");
        // Quitting while the cursor is idle-hidden would leak a global
        // ShowCursor(FALSE) past process death (winit only restores on
        // change, and there is no later present). Restore only when the
        // OS reports hidden so a balanced counter is never disturbed.
        #[cfg(windows)]
        if self.shell_cursor_hidden && !crate::platform::win32::cursor_visible().unwrap_or(true) {
            crate::controller::hid::diag::diag_info("ui-diag: exit cursor restore");
            unsafe { crate::platform::win32::show_cursor(true) };
        }
        // A renderer-pref restart keeps its restore record; a plain
        // Quit clears it so the next launch starts clean.
        if !self.restarting_for_renderer {
            crate::persist::restart_ui::clear();
        }
        self.session.known.save();
        // Drop every Launcher image Handle before iced tears down the GPU atlas.
        self.launcher_visible = false;
        self.launcher_nav_ready = false;
        self.launcher_state.art_awaiting_paint = false;
        self.launcher_state.clear_immersive_session();
        for row in &mut self.launcher_state.rows {
            row.icon = None;
        }
        let _ = crate::ui::launcher::icon_cache::bump_art_warm_generation();
        self.art.shutdown();
        // Persist the tab strip (backstop — mutations already save on the
        // way; this covers a crash-free quit with no prior change).
        self.browser.panel.save_tabs();
        // Idempotent with `InputEdgeBindGuard::drop` (subscription
        // teardown also clears).
        self.input_hub.clear();
        self.hid_worker.shutdown();
        // Close CEF browsers and let the worker ack. Bounded: a wedged
        // engine must not hang quit. Profile state is flushed continuously
        // by CEF; process exit follows immediately.
        if let Some(engine) = &self.browser.engine {
            let (tx, rx) = std::sync::mpsc::channel();
            engine.send(EngineCommand::Shutdown { done: tx });
            if rx.recv_timeout(std::time::Duration::from_secs(5)).is_err() {
                crate::controller::hid::diag::diag_info("ui-diag: engine shutdown timeout");
            }
        }
        self.tray_icon.take();
        crate::controller::hid::diag::diag_info("ui-diag: process exit complete");
        iced::exit()
    }

    // -----------------------------------------------------------------------
    // Tray
    // -----------------------------------------------------------------------

    fn create_tray(&mut self) {
        self.tray_icon = tray::create_tray(&self.session.controllers);
    }

    fn apply_tray(&mut self) -> Task<Message> {
        #[cfg(windows)]
        if let Some(handle) = self.tray_icon.as_ref() {
            tray::apply_tray(handle, &self.session.controllers);
        }
        #[cfg(not(windows))]
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

        if self.home_visible {
            self.sync_home_controllers();
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
        // While the tray is empty, retry on the fast presence cadence so 0→1 open is
        // snappy even when the OS watcher missed the device (present before launch).
        let unread_retry = self.session.controllers.is_empty()
            && self.last_battery_poll.elapsed()
                >= if self.session.prefs.home_enabled {
                    PRESENCE_INTERVAL_EMPTY
                } else {
                    UNREAD_RETRY_INTERVAL
                };

        if membership_changed || battery_due || liveness_due || unread_retry {
            // Request a refresh when membership/battery/liveness is due. The lightbar
            // reassert itself is gated downstream (new pad / color change / 30s
            // backstop), so other HID writers cannot keep the bar after a one-shot
            // overwrite without churning every liveness tick.
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

        let previously_connected: Vec<Serial> = self
            .session
            .controllers
            .iter()
            .map(|c| c.serial.clone())
            .collect();
        let worker = self.hid_worker.clone();
        Task::perform(worker.poll(previously_connected), Message::PollResult)
    }

    fn on_poll_result(&mut self, result: AppResult<Vec<ControllerStatus>>) -> Task<Message> {
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
                    home_visible: self.home_visible,
                    managed_app_visible: self.browser_visible || self.launcher_visible,
                    fullscreen: launcher_input::foreground_is_exclusive_fullscreen(),
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
                    open_home_after_toast,
                } => {
                    tasks.push(self.queue_notifications(events, open_home_after_toast));
                }
                SessionEffect::OpenHome => {
                    if crate::platform::debug_quiet::input_dropped() {
                        // Automated/quiet sessions must not pop Home over the
                        // surface under test; replay drives its own window.
                        crate::controller::hid::diag::diag_info(
                            "ui-diag: OpenHome ignored (automated input lock)",
                        );
                    } else if self.browser_visible || self.launcher_visible {
                        // Home is a peer screen: a controller connect must not
                        // interrupt a running managed app.
                        crate::controller::hid::diag::diag_info(
                            "ui-diag: OpenHome ignored (managed app visible)",
                        );
                        self.session.auto_open_pending = false;
                        self.toast_machine.clear_after();
                    } else if self.toast_machine.suppresses_reopen_gesture() {
                        // The reopen chord may fire during the connect-toast
                        // latch; the toast's own AfterToast::OpenHome still opens
                        // Home.
                        crate::controller::hid::diag::diag_info(
                            "ui-diag: OpenHome ignored (toast OpenHome pending)",
                        );
                    } else {
                        tasks.push(self.open_home());
                    }
                }
                SessionEffect::CloseHome => {
                    // Auto-close owns Home, not a manually opened Launcher.
                    if self.home_visible {
                        tasks.push(self.close_home());
                    } else {
                        tasks.push(self.close_launcher());
                    }
                }
                SessionEffect::ClearHomeLatch => {
                    self.toast_machine.clear_after();
                }
                SessionEffect::SaveKnown => {
                    self.session.known.save();
                }
                SessionEffect::SyncAnalytics => {
                    self.session.analytics.save();
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

    /// True when live pad-input edges should drive Launcher / diagnostics.
    fn wants_pad_input(&self) -> bool {
        // Debug quiet mode: HID input is focus-independent, so a pad press during
        // a game must never drive Launcher (navigate / launch). `SDSC_DEBUG_INPUT=1`
        // is the QA opt-in that keeps quiet placement but accepts pad edges.
        if crate::platform::debug_quiet::input_dropped() {
            return false;
        }
        let pad_input_live =
            self.configure_window.is_some() && self.configure_state.section == Section::PadInput;
        pad_input_live
            || self.home_visible
            || self.browser_visible
            || (self.launcher_visible
                && self.session.prefs.home_enabled
                && !self.session.controllers.is_empty())
    }

    /// Apply a controller snapshot through the session (used by presence clear + emulate).
    fn apply_controllers(&mut self, controllers: Vec<ControllerStatus>) -> Task<Message> {
        let ctx = crate::session::ApplyContext {
            home_visible: self.home_visible,
            managed_app_visible: self.browser_visible || self.launcher_visible,
            fullscreen: launcher_input::foreground_is_exclusive_fullscreen(),
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

    fn identify(&self, serial: &Serial) -> bool {
        if is_emulated_serial(serial.as_ref()) {
            app_log::info(format!("identify skipped for emulated controller {serial}"));
            return false;
        }
        // A pad on its way out takes no more commands.
        if self.launcher_state.powering_off.as_ref() == Some(serial) {
            return false;
        }

        let Some(controller) = self
            .session
            .controllers
            .iter()
            .find(|c| c.serial == *serial)
        else {
            return false;
        };
        if !controller.capabilities.lightbar {
            return false;
        }

        self.hid_worker.identify(serial.clone(), controller.percent)
    }

    fn power_off(&mut self, serial: &Serial) {
        if is_emulated_serial(serial.as_ref()) {
            app_log::info(format!(
                "power-off skipped for emulated controller {serial}"
            ));
            return;
        }
        // Already going away: don't re-send the feature report on repeat holds.
        if self.launcher_state.powering_off.as_ref() == Some(serial) {
            return;
        }

        let Some(controller) = self
            .session
            .controllers
            .iter()
            .find(|c| c.serial == *serial)
        else {
            return;
        };
        if !controller.capabilities.power_off || !controller.connection.is_bluetooth() {
            app_log::warn(format!(
                "power-off ignored for {serial} ({})",
                controller.connection
            ));
            return;
        }

        // Show "Powering off" immediately (cleared when the pad leaves the list).
        self.launcher_state.powering_off = Some(serial.clone());
        // Reconnect after user power-off should open Launcher again (not ghost-flap cooldown),
        // but only when this was the last pad — a sibling must not look like a fresh 0→1.
        if crate::session::should_skip_connect_cooldown_on_power_off(
            &self.session.controllers,
            serial.as_ref(),
            self.session.prefs.home_auto_open.includes_usb(),
        ) {
            self.session.mark_skip_connect_cooldown();
        }
        self.hid_worker.power_off(serial.clone());
    }

    fn toggle_remember(&mut self, serial: &Serial) -> Task<Message> {
        #[cfg(debug_assertions)]
        if is_emulated_serial(serial.as_ref()) {
            // Emulated pads ride the fleet path (`apply_emulator_command`
            // with `SetRemembered`); the popup pin is a no-op here.
            return Task::none();
        }
        #[cfg(not(debug_assertions))]
        if is_emulated_serial(serial.as_ref()) {
            return Task::none();
        }

        // The session is the authority; `controllers.json` is written on change.
        if self.session.known.is_remembered(serial.as_ref()) {
            self.session.known.forget(serial.as_ref());
        } else if let Some(controller) = self
            .session
            .controllers
            .iter()
            .find(|c| c.serial == *serial)
        {
            self.session.known.remember(controller);
        } else {
            return Task::none();
        }

        self.session.known.save();
        self.sync_popup_rows_and_fit()
    }

    fn set_nickname(&mut self, serial: &Serial, nickname: Option<String>) -> Task<Message> {
        #[cfg(debug_assertions)]
        if is_emulated_serial(serial.as_ref()) {
            // Emulated-pad nicknames ride the fleet path; the direct pin is
            // a no-op here.
            return Task::none();
        }
        #[cfg(not(debug_assertions))]
        if is_emulated_serial(serial.as_ref()) {
            return Task::none();
        }
        if self
            .session
            .known
            .set_nickname(serial.as_ref(), nickname.clone())
        {
            self.session.known.save();
            self.sync_popup_rows_and_fit()
        } else {
            Task::none()
        }
    }

    // -----------------------------------------------------------------------
    // Launcher
    // -----------------------------------------------------------------------

    fn queue_notifications(
        &mut self,
        mut events: Vec<NotifyEvent>,
        mut open_start_after: bool,
    ) -> Task<Message> {
        let mut connect_open_start =
            open_start_after && events.iter().any(|event| event.body == "Connected");
        if connect_open_start && (self.browser_visible || self.launcher_visible) {
            crate::controller::hid::diag::diag_info(
                "ui-diag: auto-open suppressed (managed app visible)",
            );
            connect_open_start = false;
        }
        // Auto-open never shows a Connected toast (cold-launch via
        // launch_quiet and hot connects must match). Session already filters,
        // but drop any that slipped through so behaviour is bullet-proof.
        if connect_open_start {
            let before = events.len();
            events.retain(|event| event.body != "Connected");
            if events.len() != before {
                crate::controller::hid::diag::diag_info(
                    "ui-diag: connect toast suppressed (immersive auto-open)",
                );
            }
            let open = self.open_home();
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
        // Auto-open is toast-first: latch Home onto the toast settle so
        // the Connected card always presents (the toast machine emits OpenHome
        // exactly once — on rest, early dismiss, or slide-out finish).
        for event in events {
            let eta = self.toast_eta_for(&event);
            let mut message =
                ToastMessage::from_notification(event, self.session.prefs.spectrum.clone(), eta);
            if open_start_after && message.body == "Connected" {
                open_start_after = false;
                message.after = AfterToast::OpenHome;
                crate::controller::hid::diag::diag_info(
                    "ui-diag: defer home until toast slide settles",
                );
            }
            ToastMessage::enqueue(&mut self.toast_queue, message);
        }
        self.show_next_toast()
    }

    /// Coarse window identity for the debug redraw counters (Phase 0 / WO-8).
    #[cfg(debug_assertions)]
    fn redraw_window_kind(&self, window: window::Id) -> crate::platform::redraw_stats::WindowKind {
        use crate::platform::redraw_stats::WindowKind;
        if Some(window) == self.popup_window {
            WindowKind::Popup
        } else if Some(window) == self.configure_window {
            WindowKind::Settings
        } else if Some(window) == self.osk.window {
            WindowKind::Osk
        } else if Some(window) == self.browser_window {
            WindowKind::Browser
        } else if Some(window) == self.home_window {
            WindowKind::Home
        } else if Some(window) == self.launcher_window {
            WindowKind::Launcher
        } else if Some(window) == self.toast_window {
            WindowKind::Toast
        } else if Some(window) == self.preview_window {
            WindowKind::Preview
        } else {
            WindowKind::Shell
        }
    }
}

/// Debug-only source class for the idle-redraw counters.
#[cfg(debug_assertions)]
fn redraw_message_source(message: &Message) -> crate::platform::redraw_stats::MessageSource {
    use crate::platform::redraw_stats::MessageSource as Source;
    match message {
        Message::PadInput(_) => Source::PadInput,
        Message::LauncherFrame => Source::LauncherFrame,
        Message::ToastFrame => Source::ToastFrame,
        Message::IdentifyFrame => Source::IdentifyFrame,
        Message::HomeProgressTick => Source::HomeProgress,
        Message::HomeControllerTick => Source::HomeController,
        Message::HomeFrame => Source::Home,
        Message::ShellFadeTick => Source::ShellFade,
        Message::UtilityAmbientTick => Source::UtilityAmbient,
        Message::EmulatorCommitTick => Source::EmulatorCommit,
        Message::BrowserDebugShotTick => Source::BrowserFrame,
        Message::BrowserHistoryTimeout => Source::BrowserFrame,
        Message::AmbientPreviewTick => Source::AmbientPreview,
        Message::Tick => Source::Tick,
        Message::PresenceHint | Message::AbsenceRecheck => Source::PresenceHint,
        Message::PollResult(_) => Source::PollResult,
        Message::LauncherArtEvent(_) => Source::LauncherArt,
        Message::BrowserEngine(_) => Source::BrowserEngine,
        Message::Osk(_) => Source::Osk,
        Message::Browser(_) | Message::BrowserRawKey { .. } | Message::BrowserIme { .. } => {
            Source::Browser
        }
        Message::Home(_) => Source::Home,
        Message::Launcher(_) | Message::LauncherKey { .. } | Message::LauncherIdleTick => {
            Source::Launcher
        }
        _ => Source::Other,
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
/// Per-stream arm/wake state for the ownership filters.
#[derive(Debug, Clone, Copy, Default)]
struct EdgeWakeState {
    stick: bool,
    digital: bool,
}

/// OSK-open edge filter: returns `(forward_to_ui, next_state)`.
///
/// Analog frames feed the slot only; the UI needs a wake on stick
/// transitions, actions, non-sample snapshots, and D-pad/button changes.
/// Held D-pad repeats are handled by the pull-model widget.
fn osk_stream_wake(
    edge: &crate::domain::pad::InputEdge,
    state: EdgeWakeState,
) -> (bool, EdgeWakeState) {
    let sample_reason = matches!(
        edge.reason,
        crate::domain::pad::SnapshotReason::Sample
            | crate::domain::pad::SnapshotReason::SamplePartial
    );
    let stick = edge
        .readings
        .iter()
        .any(|reading| sample_has_stick(&reading.sample));
    let digital = edge
        .readings
        .iter()
        .any(|reading| sample_has_digital(&reading.sample));
    let ps_held = edge.readings.iter().any(|reading| {
        reading
            .sample
            .held
            .contains(&crate::domain::gesture::GestureControl::Ps)
    });
    let wake = !sample_reason
        || !edge.actions.is_empty()
        || ps_held
        || digital != state.digital
        || stick != state.stick;
    (wake, EdgeWakeState { stick, digital })
}

/// Browser edge filter: the cursor pulls the latest snapshot, so drop
/// stick-only steady frames. Digital controls stay on the edge stream because
/// the Browser D-pad stepper repeats from those frames.
fn browser_stream_wake(
    edge: &crate::domain::pad::InputEdge,
    state: EdgeWakeState,
) -> (bool, EdgeWakeState) {
    let sample_reason = matches!(
        edge.reason,
        crate::domain::pad::SnapshotReason::Sample
            | crate::domain::pad::SnapshotReason::SamplePartial
    );
    let stick = edge
        .readings
        .iter()
        .any(|reading| sample_has_stick(&reading.sample));
    let digital = edge
        .readings
        .iter()
        .any(|reading| sample_has_digital(&reading.sample));
    let ps_held = edge.readings.iter().any(|reading| {
        reading
            .sample
            .held
            .contains(&crate::domain::gesture::GestureControl::Ps)
    });
    let wake = !sample_reason
        || !edge.actions.is_empty()
        || ps_held
        || digital
        || digital != state.digital
        || edge.analog_changed
        || stick != state.stick;
    (wake, EdgeWakeState { stick, digital })
}

fn sample_has_stick(sample: &crate::domain::pad::PadSample) -> bool {
    sample.lstick_x != 0.0
        || sample.lstick_y != 0.0
        || sample.rstick_x != 0.0
        || sample.rstick_y != 0.0
}

fn sample_has_digital(sample: &crate::domain::pad::PadSample) -> bool {
    !sample.held.is_empty()
        || sample.cross
        || sample.circle
        || sample.square
        || sample.triangle
        || sample.options
        || sample.create
        || sample.l1
        || sample.r1
        || sample.l2
        || sample.r2
        || sample.l3
        || sample.dpad_up
        || sample.dpad_down
        || sample.dpad_left
        || sample.dpad_right
}

fn pad_input_edge_stream(hub: &Arc<InputHub>) -> impl Stream<Item = Message> + Send + use<> {
    // Deep enough to ride out Launcher-frame hitches without dropping taps:
    // the worker publishes at report rate while hot.
    let hub = Arc::clone(hub);
    stream::channel(128, async move |mut output| {
        let (tx, mut rx) = iced::futures::channel::mpsc::channel(128);
        hub.bind(tx);
        let _guard = InputEdgeBindGuard { hub: hub.clone() };
        let mut osk_state = EdgeWakeState::default();
        let mut browser_state = EdgeWakeState::default();
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
            let mut actions = std::mem::take(&mut edge.actions);
            while let Ok(next) = rx.try_recv() {
                edge = next;
                actions.append(&mut edge.actions);
            }
            edge.actions = actions;
            // OSK open: analog frames stay in the pull-model slot. Only
            // forward a wake on rest<->active transitions and while PS is
            // held (global Home chord); actions always pass immediately.
            if hub.osk_listening() {
                let (wake, next) = osk_stream_wake(&edge, osk_state);
                osk_state = next;
                if !wake {
                    continue;
                }
            } else if hub.browser_listening() {
                // Browser cursor is pull-model: analog-only frames stay in
                // the snapshot. Digital frames stay on the edge stream so the
                // D-pad stepper repeat cadence works.
                let (wake, next) = browser_stream_wake(&edge, browser_state);
                browser_state = next;
                if !wake {
                    continue;
                }
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
        // unbound Home is a no-op).
        self.hub.clear();
    }
}

/// CEF engine worker → iced event bridge.
///
/// Two transports merge here (see [`EngineHandle`]): frames conflate to the
/// latest buffered one (paint output is absolute state, like pad
/// snapshots); control events (title/URL/load/focus/tree/tabs) arrive
/// lossless and in order. Bind/clear mirrors the pad edge Home.
fn engine_event_stream(engine: &EngineHandle) -> impl Stream<Item = Message> + Send + use<> {
    use iced::futures::StreamExt;
    let engine = engine.clone();
    stream::channel(64, async move |mut output| {
        let (frame_tx, mut frame_rx) =
            iced::futures::channel::mpsc::channel::<crate::browser::engine::TabFrame>(64);
        let (ctrl_tx, mut ctrl_rx) = iced::futures::channel::mpsc::unbounded::<EngineEvent>();
        engine.bind(frame_tx, ctrl_tx);
        let _guard = EngineBindGuard { engine };
        loop {
            iced::futures::select! {
                control = ctrl_rx.next() => {
                    let Some(event) = control else { break };
                    if output.send(Message::BrowserEngine(event)).await.is_err() {
                        break;
                    }
                }
                frame = frame_rx.next() => {
                    let Some(first) = frame else { break };
                    // Drain any newer frames; only the latest paints.
                    let mut latest = first;
                    while let Ok(newer) = frame_rx.try_recv() {
                        latest = newer;
                    }
                    if output
                        .send(Message::BrowserEngine(EngineEvent::Frame(latest)))
                        .await
                        .is_err()
                    {
                        break;
                    }
                }
            }
        }
    })
}

struct EngineBindGuard {
    engine: EngineHandle,
}
impl Drop for EngineBindGuard {
    fn drop(&mut self) {
        self.engine.clear();
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

/// Hitch context: the owned input Home snapshot (`InputHub`) is the
/// single source F7/F8 stamps read from (safe off the UI thread).
#[cfg(debug_assertions)]
fn stamp_hitch_report(hub: &InputHub, kind: &str, source: &str, extra: Option<String>) {
    let (pads, reason, age_ms, seq) = match hub.read() {
        launcher_input::NavReadingsOutcome::Readings { readings, meta } => (
            readings.len(),
            meta.reason.as_str(),
            meta.published_at.elapsed().as_millis(),
            meta.seq,
        ),
        launcher_input::NavReadingsOutcome::Missing { meta } => (
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
    // Automated sessions must not reserve F7/F8 globally.
    if crate::platform::debug_quiet::input_dropped() {
        return;
    }
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
fn spawn_blocking<T, F>(f: F) -> impl Future<Output = AppResult<T>> + Send
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
            .map_err(|_| AppError::Ui("background task was cancelled".into()))
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
        assert!(!Message::RendererReloadDue.is_user_input());
    }

    #[test]
    fn osk_stream_wakes_only_on_transitions_ps_and_actions() {
        use crate::domain::gesture::GestureControl;
        use crate::domain::pad::{InputEdge, PadId, PadSample, SnapshotMeta, SnapshotReason};
        let meta = SnapshotMeta {
            published_at: std::time::Instant::now(),
            seq: 1,
            reason: SnapshotReason::Sample,
            pad_count: 1,
        };
        let edge = |sample: PadSample, actions: Vec<crate::domain::pad::PadAction>| {
            InputEdge::from_snapshot(
                vec![crate::domain::pad::NavReading {
                    id: PadId("hid:a".into()),
                    sample,
                    source: "hid",
                }],
                actions,
                &meta,
            )
        };
        let mut state = EdgeWakeState::default();
        let (wake, next) = osk_stream_wake(&edge(PadSample::default(), Vec::new()), state);
        assert!(!wake, "resting neutral stays in the slot");
        assert!(!next.stick && !next.digital);
        let held = PadSample {
            lstick_x: -0.95,
            stick_x: -0.95,
            ..Default::default()
        };
        let (wake, next) = osk_stream_wake(&edge(held.clone(), Vec::new()), state);
        assert!(wake, "rest->active wake");
        state = next;
        let (wake, next) = osk_stream_wake(&edge(held.clone(), Vec::new()), state);
        assert!(!wake, "held analog frames do not cross");
        assert!(next.stick);
        let (wake, next) = osk_stream_wake(&edge(PadSample::default(), Vec::new()), state);
        assert!(wake, "release wake");
        assert!(!next.stick);
        let ps = PadSample {
            held: [GestureControl::Ps].into_iter().collect(),
            ..Default::default()
        };
        let (wake, _) = osk_stream_wake(&edge(ps, Vec::new()), EdgeWakeState::default());
        assert!(wake, "PS stays global");
        let (wake, _) = osk_stream_wake(
            &edge(
                PadSample::default(),
                vec![crate::domain::pad::PadAction {
                    pad: PadId("hid:a".into()),
                    button: crate::domain::pad::PadButton::Cross,
                    action: crate::domain::pad::PadButtonAction::Tap,
                }],
            ),
            EdgeWakeState::default(),
        );
        assert!(wake, "taps always publish immediately");
        let dpad = PadSample {
            dpad_up: true,
            ..Default::default()
        };
        let (wake, next) = osk_stream_wake(&edge(dpad.clone(), Vec::new()), state);
        assert!(wake, "first D-pad press wakes the pull widget");
        assert!(next.digital);
        let (wake, _) = osk_stream_wake(&edge(dpad, Vec::new()), next);
        assert!(!wake, "held D-pad repeats are widget-owned");
    }

    #[test]
    fn browser_stream_keeps_digital_but_drops_analog_only_frames() {
        use crate::domain::pad::{InputEdge, PadId, PadSample, SnapshotMeta, SnapshotReason};
        let meta = SnapshotMeta {
            published_at: std::time::Instant::now(),
            seq: 1,
            reason: SnapshotReason::Sample,
            pad_count: 1,
        };
        let edge = |sample: PadSample| {
            InputEdge::from_snapshot(
                vec![crate::domain::pad::NavReading {
                    id: PadId("hid:a".into()),
                    sample,
                    source: "hid",
                }],
                Vec::new(),
                &meta,
            )
        };
        let held = PadSample {
            lstick_x: -0.95,
            stick_x: -0.95,
            ..Default::default()
        };
        let (wake, state) = browser_stream_wake(&edge(held.clone()), EdgeWakeState::default());
        assert!(wake, "stick start wakes the canvas");
        assert!(state.stick);
        assert!(!state.digital);
        let (wake, _) = browser_stream_wake(&edge(held), state);
        assert!(!wake, "held analog stays in the snapshot");
        let dpad = PadSample {
            dpad_up: true,
            ..Default::default()
        };
        let (wake, state) = browser_stream_wake(&edge(dpad.clone()), EdgeWakeState::default());
        assert!(wake, "D-pad press must reach the jump stepper");
        assert!(!state.stick);
        assert!(state.digital);
        let (wake, _) = browser_stream_wake(&edge(dpad), state);
        assert!(wake, "D-pad repeat stays on the edge stream");
        let (wake, state) = browser_stream_wake(&edge(PadSample::default()), state);
        assert!(wake, "D-pad release wakes the stepper");
        assert!(!state.digital);
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
        let edited = emulate::update(&before, &Serial::from("emu-1"), |p| p.remembered = false);
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
            &Serial::from("emu-1"),
            |p| p.connected = true,
        );
        emulate::sync_remembered_emulated(&[], &plugged, &mut known);

        // Forget while connected: stays in the fleet, drops from the store.
        let forgotten = emulate::update(&plugged, &Serial::from("emu-1"), |p| p.remembered = false);
        assert_eq!(emulate::prune(forgotten.clone()).len(), 1, "live pads stay");
        emulate::sync_remembered_emulated(&plugged, &forgotten, &mut known);
        assert!(!known.is_remembered("emu-1"));

        // Now unplug it: prune removes it, and it must stay out of the store.
        let unplugged = emulate::prune(emulate::update(&forgotten, &Serial::from("emu-1"), |p| {
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
            &Serial::from("emu-1"),
            |p| p.connected = true,
        );
        let unplugged = emulate::prune(emulate::update(&plugged, &Serial::from("emu-1"), |p| {
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

        let edited = emulate::update(&both, &Serial::from("emu-1"), |p| p.remembered = false);
        let after = emulate::prune(edited);
        emulate::sync_remembered_emulated(&both, &after, &mut known);

        assert!(!known.is_remembered("emu-1"));
        assert!(known.is_remembered("emu-2"), "the other pad is untouched");
    }
}
