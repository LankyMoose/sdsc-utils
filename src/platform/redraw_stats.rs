//! Debug-only per-window redraw + message-source counters.
//!
//! iced 0.14 rebuilds and requests a redraw for every window after any
//! message, so the only practical way to prove "nothing is ticking" is to
//! count `view` rebuilds per window and `Message` dispatches by source.
//!
//! Opt-in with `SDSC_DEBUG_REDRAW=1`: a plain OS thread appends a cumulative
//! snapshot to `app.log` every [`SNAPSHOT_INTERVAL`] without touching the iced
//! event loop (a timer subscription would itself defeat the measurement).
//! QA reads the `ui-redraw:` lines and compares deltas between snapshots.
//! Compiled out of release builds.

use crate::platform::app_log;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

const SNAPSHOT_INTERVAL: Duration = Duration::from_secs(2);

pub const VIEW_WINDOWS: [&str; 9] = [
    "popup", "settings", "osk", "browser", "home", "launcher", "toast", "preview", "shell",
];

pub const MESSAGE_SOURCES: [&str; 23] = [
    "pad_input",
    "launcher_frame",
    "browser_frame",
    "toast_frame",
    "identify_frame",
    "home_progress",
    "home_controller",
    "shell_fade",
    "renderer_reload",
    "update_spin",
    "utility_ambient",
    "emulator_commit",
    "ambient_preview",
    "tick",
    "presence_hint",
    "poll_result",
    "launcher_art",
    "browser_engine",
    "osk",
    "browser",
    "home",
    "launcher",
    "other",
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WindowKind {
    Popup,
    Settings,
    Osk,
    Browser,
    Home,
    Launcher,
    Toast,
    Preview,
    Shell,
}

impl WindowKind {
    fn index(self) -> usize {
        match self {
            Self::Popup => 0,
            Self::Settings => 1,
            Self::Osk => 2,
            Self::Browser => 3,
            Self::Home => 4,
            Self::Launcher => 5,
            Self::Toast => 6,
            Self::Preview => 7,
            Self::Shell => 8,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MessageSource {
    PadInput,
    LauncherFrame,
    BrowserFrame,
    ToastFrame,
    IdentifyFrame,
    HomeProgress,
    HomeController,
    ShellFade,
    RendererReload,
    UpdateSpin,
    UtilityAmbient,
    EmulatorCommit,
    AmbientPreview,
    Tick,
    PresenceHint,
    PollResult,
    LauncherArt,
    BrowserEngine,
    Osk,
    Browser,
    Home,
    Launcher,
    Other,
}

impl MessageSource {
    fn index(self) -> usize {
        match self {
            Self::PadInput => 0,
            Self::LauncherFrame => 1,
            Self::BrowserFrame => 2,
            Self::ToastFrame => 3,
            Self::IdentifyFrame => 4,
            Self::HomeProgress => 5,
            Self::HomeController => 6,
            Self::ShellFade => 7,
            Self::RendererReload => 8,
            Self::UpdateSpin => 9,
            Self::UtilityAmbient => 10,
            Self::EmulatorCommit => 11,
            Self::AmbientPreview => 12,
            Self::Tick => 13,
            Self::PresenceHint => 14,
            Self::PollResult => 15,
            Self::LauncherArt => 16,
            Self::BrowserEngine => 17,
            Self::Osk => 18,
            Self::Browser => 19,
            Self::Home => 20,
            Self::Launcher => 21,
            Self::Other => 22,
        }
    }
}

static REDRAWS: [AtomicU64; VIEW_WINDOWS.len()] = [
    AtomicU64::new(0),
    AtomicU64::new(0),
    AtomicU64::new(0),
    AtomicU64::new(0),
    AtomicU64::new(0),
    AtomicU64::new(0),
    AtomicU64::new(0),
    AtomicU64::new(0),
    AtomicU64::new(0),
];

static WINDOW_KINDS: std::sync::Mutex<Option<std::collections::HashMap<iced::window::Id, usize>>> =
    std::sync::Mutex::new(None);

static VIEWS: [AtomicU64; VIEW_WINDOWS.len()] = [
    AtomicU64::new(0),
    AtomicU64::new(0),
    AtomicU64::new(0),
    AtomicU64::new(0),
    AtomicU64::new(0),
    AtomicU64::new(0),
    AtomicU64::new(0),
    AtomicU64::new(0),
    AtomicU64::new(0),
];

static MESSAGES: [AtomicU64; MESSAGE_SOURCES.len()] = [
    AtomicU64::new(0),
    AtomicU64::new(0),
    AtomicU64::new(0),
    AtomicU64::new(0),
    AtomicU64::new(0),
    AtomicU64::new(0),
    AtomicU64::new(0),
    AtomicU64::new(0),
    AtomicU64::new(0),
    AtomicU64::new(0),
    AtomicU64::new(0),
    AtomicU64::new(0),
    AtomicU64::new(0),
    AtomicU64::new(0),
    AtomicU64::new(0),
    AtomicU64::new(0),
    AtomicU64::new(0),
    AtomicU64::new(0),
    AtomicU64::new(0),
    AtomicU64::new(0),
    AtomicU64::new(0),
    AtomicU64::new(0),
    AtomicU64::new(0),
];

static START: OnceLock<Instant> = OnceLock::new();
static ENABLED: AtomicBool = AtomicBool::new(false);

/// Start the opt-in snapshot writer. No-op (and no thread) unless
/// `SDSC_DEBUG_REDRAW` is truthy. Call once after `app_log::init`.
pub fn init() {
    if !env_enabled() || START.set(Instant::now()).is_err() {
        return;
    }
    ENABLED.store(true, Ordering::Relaxed);
    let _ = std::thread::Builder::new()
        .name("ui-redraw-stats".into())
        .spawn(|| {
            let start = *START.get().unwrap_or(&Instant::now());
            loop {
                std::thread::sleep(SNAPSHOT_INTERVAL);
                snapshot_at("periodic", start.elapsed());
            }
        });
}

/// Log one cumulative snapshot now (also the QA bracket hook).
pub fn snapshot(reason: &str) {
    if !ENABLED.load(Ordering::Relaxed) {
        return;
    }
    let elapsed = START.get().map(Instant::elapsed).unwrap_or(Duration::ZERO);
    snapshot_at(reason, elapsed);
}

fn snapshot_at(reason: &str, elapsed: Duration) {
    let views: Vec<String> = VIEW_WINDOWS
        .iter()
        .zip(VIEWS.iter())
        .map(|(name, counter)| format!("{name}={}", counter.load(Ordering::Relaxed)))
        .collect();
    let messages: Vec<String> = MESSAGE_SOURCES
        .iter()
        .zip(MESSAGES.iter())
        .map(|(name, counter)| format!("{name}={}", counter.load(Ordering::Relaxed)))
        .collect();
    let redraws: Vec<String> = VIEW_WINDOWS
        .iter()
        .zip(REDRAWS.iter())
        .map(|(name, counter)| format!("{name}={}", counter.load(Ordering::Relaxed)))
        .collect();
    app_log::info(format!(
        "ui-redraw: reason={reason} t_ms={} views[{}] redraws[{}] msgs[{}]",
        elapsed.as_millis(),
        views.join(" "),
        redraws.join(" "),
        messages.join(" "),
    ));
}

fn env_enabled() -> bool {
    matches!(
        std::env::var("SDSC_DEBUG_REDRAW").as_deref(),
        Ok("1") | Ok("true") | Ok("yes")
    )
}

pub fn note_view(window: iced::window::Id, kind: WindowKind) {
    if ENABLED.load(Ordering::Relaxed) {
        VIEWS[kind.index()].fetch_add(1, Ordering::Relaxed);
        if let Ok(mut guard) = WINDOW_KINDS.lock() {
            guard
                .get_or_insert_with(std::collections::HashMap::new)
                .insert(window, kind.index());
        }
    }
}

/// Count an actual `RedrawRequested` for a window (present/repaint proof).
pub fn note_redraw(window: iced::window::Id) {
    if !ENABLED.load(Ordering::Relaxed) {
        return;
    }
    let index = WINDOW_KINDS
        .lock()
        .ok()
        .and_then(|guard| guard.as_ref().and_then(|map| map.get(&window).copied()));
    if let Some(index) = index
        && let Some(counter) = REDRAWS.get(index)
    {
        counter.fetch_add(1, Ordering::Relaxed);
    }
}

pub fn note_message(source: MessageSource) {
    if ENABLED.load(Ordering::Relaxed) {
        MESSAGES[source.index()].fetch_add(1, Ordering::Relaxed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn windows_and_sources_match_their_indices() {
        assert_eq!(WindowKind::Shell.index(), VIEW_WINDOWS.len() - 1);
        assert_eq!(MessageSource::Other.index(), MESSAGE_SOURCES.len() - 1);
        for (index, kind) in [
            WindowKind::Popup,
            WindowKind::Settings,
            WindowKind::Osk,
            WindowKind::Browser,
            WindowKind::Home,
            WindowKind::Launcher,
            WindowKind::Toast,
            WindowKind::Preview,
            WindowKind::Shell,
        ]
        .into_iter()
        .enumerate()
        {
            assert_eq!(kind.index(), index);
        }
    }
}
