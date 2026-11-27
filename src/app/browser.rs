//! Top-level Browser app state.
//!
//! Owns the standalone Browser window: panel, engine handle, capability flag,
//! pad/key routing, URL editor, and the browser-only D-pad stepper. The window
//! renders a one-row header (Back / URL / Forward) above the CEF page; see
//! `notes/browser-embedding.md` for the engine/cursor contract.

use crate::browser::engine::{EngineCommand, EngineHandle};
use crate::domain::pad::{
    NavAction, NavBank, NavStepper, PadAction, PadButton, PadButtonAction, PadId,
};
use crate::platform::ui_sound::UiSoundKind;
use crate::ui::browser::BrowserPanel;
use std::path::PathBuf;

/// Result of a background local install/update.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EngineInstallOutcome {
    Installed(PathBuf),
    /// The user asked to cancel; staging was cleaned and the previous state
    /// is restored.
    Cancelled,
    Failed(String),
}

/// Install/runtime state for the optional CEF engine package.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
// UI-only builds construct only `Missing`; the install states are exercised
// with `cef-engine` enabled.
#[allow(dead_code)]
pub enum EngineStatus {
    /// No valid runtime found. Home Browser tile offers install.
    #[default]
    Missing,
    /// Runtime directory verified and the worker is live.
    Ready(PathBuf),
    /// An install/download is in flight.
    Installing,
    /// A local uninstall is in flight.
    Uninstalling,
    /// The last install attempt failed; message is user-facing.
    Failed(String),
}

impl EngineStatus {
    pub fn is_ready(&self) -> bool {
        matches!(self, Self::Ready(_))
    }
}

/// EWMA sample used to render a stable live throughput while bytes move.
#[cfg(feature = "cef-engine")]
#[derive(Debug, Clone, Copy)]
pub struct EngineRateSample {
    pub at: Instant,
    pub bytes: u64,
    pub ewma_bps: f64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum BrowserTraceKind {
    History,
    Navigate,
}

/// In-flight browser navigation/history action whose real outcome is the
/// settled engine state, not the command send.
pub(crate) struct PendingBrowserTrace {
    pub action: crate::platform::trace::PendingAction,
    pub kind: BrowserTraceKind,
    pub requested_index: Option<usize>,
    pub requested_url: Option<String>,
    /// Load generation for `Navigate`; history traversals have none.
    pub generation: Option<u64>,
}

/// A history action that needs CEF's canonical snapshots to settle.
///
/// `target_index = None` is the L1/R1 contract: one CEF traversal step, accept
/// wherever Chromium lands. A selector pick uses `target_index = Some(..)`
/// and jumps directly to that index through the DevTools protocol, so
/// Chromium's skippable-entry filter cannot skip past it.
pub(crate) struct PendingHistoryTravel {
    pub tab: crate::browser::engine::TabId,
    pub target_index: Option<usize>,
    pub target_url: Option<String>,
    pub start_index: Option<usize>,
    pub start_url: String,
    pub started: Instant,
}

/// Event-driven replay settle gate.
///
/// A step starts its successor when the step's own signals are observed:
/// engine load/history settled, a window redraw landed, and screenshot
/// captures completed. The timeout is only a liveness backstop, never the
/// normal path.
#[derive(Debug, Clone, Default)]
pub(crate) struct ReplayWait {
    /// Timeout generation (stale timeout messages are ignored).
    pub id: u64,
    /// Wait for `pending_browser_trace` / `pending_history_travel` to clear
    /// and the active page activity to return to Idle.
    pub needs_engine: bool,
    /// Wait for one window `RedrawRequested` after the step.
    pub needs_redraw: bool,
    /// Wait for one screenshot capture to be handled.
    pub needs_capture: bool,
    /// Browser-open gate (first step only).
    pub needs_browser_ready: bool,
    /// Wait for a nonempty focusable set for the current document
    /// (cursor.dock).
    pub needs_focusables: bool,
    /// Wait for a focusable tree newer than this generation. A cursor nudge
    /// culls the next tree around the landing point; the following
    /// directional step must read that tree, not the pre-jump one.
    pub focusables_after: Option<u64>,
    /// Wait for a new CEF paint generation after a navigation.
    pub needs_frame: bool,
    pub frame_mark: u64,
    /// Wait for the latest CEF paint to belong to the current document URL.
    pub needs_current_frame: bool,
    /// Wait for a page title-change event (selection/scroll reports).
    pub needs_title: bool,
    pub title_mark: u64,
    /// Wait for the system OSK window to exist.
    pub needs_osk_ready: bool,
    /// Wait for the pull-model OSK widget to consume queued replay frames.
    pub needs_osk_idle: bool,
    /// Wait for an in-flight D-pad nudge to land.
    pub needs_cursor_idle: bool,
    /// Wait for a browser grid FLIP transition to settle.
    pub needs_grid_idle: bool,
    /// Wait for a shell mode switch / PS hide-or-resume to settle.
    pub needs_shell_idle: bool,
    pub redraw_seen: bool,
    /// Capture count at wait creation; ready once the pending count drops.
    pub captures_to_finish: u32,
    /// Short debounce for steps that may start a navigation. A load start
    /// arrives as `ActivityChanged`; this keeps a click/Enter from racing it.
    pub engine_grace_until: Option<Instant>,
}

#[derive(Default)]
pub struct BrowserAppState {
    /// Tabs, chrome targets, cursor, bookmarks, and per-tab page state.
    pub panel: BrowserPanel,
    /// CEF OSR worker. `None` in UI-only builds or when CEF is missing.
    pub engine: Option<EngineHandle>,
    /// Process-static capability: true when an engine worker is live. Drives
    /// the page placeholder copy and Home tile install state.
    pub engine_available: bool,
    /// Runtime/install status for the optional engine package.
    pub engine_status: EngineStatus,
    /// Guards live install progress against stragglers after completion.
    #[cfg(feature = "cef-engine")]
    pub engine_install_generation: u64,
    /// Cancel flag for the live install/update worker.
    #[cfg(feature = "cef-engine")]
    pub engine_install_cancel: std::sync::Arc<std::sync::atomic::AtomicBool>,
    /// State to restore when an install/update is cancelled.
    #[cfg(feature = "cef-engine")]
    pub engine_install_previous: Option<EngineStatus>,
    /// Latest fetched DLC manifest (feed metadata).
    #[cfg(feature = "cef-engine")]
    pub engine_manifest: Option<crate::browser::engine_install::EngineManifest>,
    /// Guards late manifest responses.
    #[cfg(feature = "cef-engine")]
    pub engine_manifest_generation: u64,
    /// Throughput sample for the in-tile MB/s readout.
    #[cfg(feature = "cef-engine")]
    pub engine_rate: Option<EngineRateSample>,
    /// Guards the background directory-size walk for Info.
    pub engine_info_generation: u64,
    /// Resolver selected a package-owned runtime (real MSIX or mock).
    pub engine_managed: bool,
    /// An update is on disk but the running CEF is still the old version.
    #[cfg(feature = "cef-engine")]
    pub engine_restart_required: bool,
    /// Engine-mode D-pad cursor jumps run on this standalone stepper (fed a
    /// stickless `dpad_only` mask): the Launcher bank stays parked in engine
    /// mode so stick moves never double-fire into both cursor and nav.
    pub dpad_stepper: NavStepper,
    /// Browser-window pad bank: face/bumper edges + reader nav. The engine
    /// cursor uses `dpad_stepper` and raw stick samples instead.
    pub nav: NavBank,
    /// Tabs-view navigation: the shared per-pad [`NavBank`] (menu deadzone,
    /// repeat, neutral grace, arming) so the grid moves exactly like
    /// Launcher/Home/OSK.
    pub grid_nav: NavBank,
    /// First fresh Browser surface attempted the persisted-strip restore.
    /// The engine preloads the saved active URL at boot, so "session URL is
    /// empty" is not a usable freshness test on later opens.
    pub tabs_restored: bool,
    /// Home tile open should land on the tabs view, not the current page.
    pub start_in_grid: bool,
    /// Direction pressed during the tabs-view entrance FLIP; applied once the
    /// grid is armed so the first navigation is not swallowed.
    pub pending_grid_move: Option<(crate::browser::cursor::JumpDir, PadId)>,
    /// Initial strip offset to apply on the first post-open frame (the
    /// scrollable does not exist until the grid view is mounted).
    pub grid_scroll_pending: Option<f32>,
}

use iced::Task;
use std::time::Instant;

use super::Message;

/// Silent result of closing the tabs view. The transition and its cue are
/// separate: the caller emits the one cue when a close actually started.
pub(crate) enum GridCloseOutcome {
    Started(Task<Message>),
    Rejected,
}

/// Map source-side tap actions to the Browser's semantic nav actions.
/// Tap only: no hold actions, no history selector, no hints/status.
fn browser_tap_action(actions: &[PadAction]) -> Option<(PadId, NavAction)> {
    actions.iter().find_map(|action| {
        let nav = match (action.button, action.action) {
            (PadButton::Cross, PadButtonAction::Tap) => NavAction::Confirm,
            (PadButton::Circle, PadButtonAction::Tap) => NavAction::Cancel,
            (PadButton::Square, PadButtonAction::Tap) => NavAction::CycleSort,
            (PadButton::Triangle, PadButtonAction::Tap) => NavAction::Triangle,
            (PadButton::Create, PadButtonAction::Tap) => NavAction::ToggleControllers,
            // L1/R1/Options semantic actions are resolved by the OS binding
            // table, not this generic nav map.
            (PadButton::L3, PadButtonAction::Tap) => NavAction::ToggleMagnetism,
            _ => return None,
        };
        Some((action.pad.clone(), nav))
    })
}

/// Shape per-pad readings for tabs-view focus: use the left stick only and
/// run it through the shared MENU deadzone so a light touch cannot move the
/// grid. The right stick stays available for bookmark scrolling.
fn grid_nav_readings(
    readings: &[crate::ui::launcher::input::NavReading],
) -> Vec<crate::ui::launcher::input::NavReading> {
    use crate::domain::pad::NavProfile;
    readings
        .iter()
        .map(|reading| {
            let mut reading = reading.clone();
            let (x, y) =
                NavProfile::MENU.shape_stick(reading.sample.lstick_x, reading.sample.lstick_y);
            reading.sample.stick_x = x;
            reading.sample.stick_y = y;
            reading
        })
        .collect()
}

/// Dominant direction in a pad snapshot, shaped by the same MENU profile as
/// tabs focus. Used to buffer an input that arrives while the entrance FLIP is
/// still running.
fn grid_direction(
    readings: &[crate::ui::launcher::input::NavReading],
) -> Option<(crate::browser::cursor::JumpDir, PadId)> {
    use crate::browser::cursor::JumpDir;
    let shaped = grid_nav_readings(readings);
    for reading in &shaped {
        if crate::domain::pad::sample_nav_resting(&reading.sample) {
            continue;
        }
        let s = &reading.sample;
        let x = if s.dpad_left {
            -1.0
        } else if s.dpad_right {
            1.0
        } else {
            s.stick_x
        };
        let y = if s.dpad_up {
            -1.0
        } else if s.dpad_down {
            1.0
        } else {
            s.stick_y
        };
        if x == 0.0 && y == 0.0 {
            continue;
        }
        let dir = if x.abs() > y.abs() {
            if x < 0.0 {
                JumpDir::Left
            } else {
                JumpDir::Right
            }
        } else if y < 0.0 {
            JumpDir::Up
        } else {
            JumpDir::Down
        };
        return Some((dir, reading.id.clone()));
    }
    None
}

const REPLAY_BROWSER_WAIT: std::time::Duration = std::time::Duration::from_millis(2500);
/// Liveness backstop for an event-driven replay settle. The next step normally
/// starts as soon as its engine/redraw/capture signal arrives; this only
/// prevents a missing event from hanging the flow.
const REPLAY_SETTLE_TIMEOUT: std::time::Duration = std::time::Duration::from_millis(1500);
/// Short engine debounce for input that may start a navigation (Enter, click,
/// Cross). The load-start event normally arrives within a frame; this closes
/// the dispatch-to-load race without turning into a per-step delay.
const REPLAY_NAV_GRACE: std::time::Duration = std::time::Duration::from_millis(120);
/// Final settle before `replay.done`: lets the last queued CEF input land and
/// the page title/URL flush without closing the window underneath it.
const REPLAY_FINAL_GRACE: std::time::Duration = std::time::Duration::from_millis(150);
/// Browser-open settle: the boot/default page navigation can start right after
/// the window appears. Give it one debounce window before the first replay
/// step so a restored Google load cannot supersede the replay navigate.
const REPLAY_BROWSER_GRACE: std::time::Duration = std::time::Duration::from_millis(250);
/// A selector jump that neither lands nor reports a DevTools error settles as
/// a timeout so the action cannot hang the trace or the selector state.
const HISTORY_TRAVEL_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);
/// Double/triple-click window and movement tolerance for page selection.
const MULTI_CLICK_WINDOW: std::time::Duration = std::time::Duration::from_millis(500);
const MULTI_CLICK_SLOP: f32 = 4.0;

/// Advance the double/triple-click chain. Returns the count for this press and
/// the stamp to store for the next one.
fn next_click_count(
    last: Option<(Instant, f32, f32, u8)>,
    now: Instant,
    x: f32,
    y: f32,
) -> (u8, Option<(Instant, f32, f32, u8)>) {
    let count = match last {
        Some((at, last_x, last_y, count))
            if now.saturating_duration_since(at) <= MULTI_CLICK_WINDOW
                && (x - last_x).abs() <= MULTI_CLICK_SLOP
                && (y - last_y).abs() <= MULTI_CLICK_SLOP =>
        {
            count.saturating_add(1).min(3)
        }
        _ => 1,
    };
    (count, Some((now, x, y, count)))
}

impl super::App {
    /// Replay driver tick. Opens the Browser surface first, validates stable
    /// preconditions, then dispatches the next semantic step through the same
    /// handlers a real action uses.
    ///
    /// Scheduling is event-driven: after a step the driver waits for the
    /// step's own signals (engine load/history settled, a window redraw, or a
    /// screenshot capture), not a fixed delay. `REPLAY_SETTLE_TIMEOUT` is a
    /// liveness backstop only.
    pub(crate) fn on_replay_step(&mut self) -> Task<Message> {
        let Some(flow) = self.replay.as_ref().map(|plan| plan.flow()) else {
            return Task::none();
        };
        if self.replay_wait.is_some() {
            if self.replay_wait_ready() {
                self.replay_wait = None;
            } else {
                // The frame/engine/capture event handlers kick us when ready.
                return Task::none();
            }
        }
        let wants_browser =
            self.replay.as_ref().and_then(|plan| plan.peek_app()) == Some("browser");
        let screen_step = self
            .replay
            .as_ref()
            .and_then(|plan| plan.peek_action())
            .is_some_and(|action| matches!(action, "screen.ps" | "screen.home"));
        if wants_browser && !screen_step && !self.browser_visible {
            // A PS deferral owns the next open; clearing it here would turn
            // the resume into a plain replay open (and emit no screen.resume).
            // A closing window owns the same transition until WindowClosed.
            if self.ps_resume_pending || self.browser_window.is_some() {
                return self
                    .replay_install_wait(
                        crate::app::browser::ReplayWait {
                            needs_browser_ready: true,
                            needs_engine: true,
                            needs_redraw: true,
                            ..Default::default()
                        },
                        REPLAY_BROWSER_WAIT,
                    )
                    .unwrap_or_else(Task::none);
            }
            let open = self.open_browser_window();
            let wait = crate::app::browser::ReplayWait {
                needs_browser_ready: true,
                needs_engine: true,
                engine_grace_until: Some(Instant::now() + REPLAY_BROWSER_GRACE),
                needs_redraw: true,
                ..Default::default()
            };
            let timeout = self.replay_install_wait(wait, REPLAY_BROWSER_WAIT);
            return match timeout {
                Some(timeout) => Task::batch([open, timeout]),
                None => Task::batch([open, Task::done(Message::ReplayStep)]),
            };
        }
        if !self.replay_prime_done {
            // A Browser restored/open before replay may still be loading its
            // default page. Let that settle (with a short start debounce) so
            // the first replay step is not superseded by the boot load.
            self.replay_prime_done = true;
            let wait = crate::app::browser::ReplayWait {
                needs_engine: true,
                engine_grace_until: Some(Instant::now() + REPLAY_NAV_GRACE),
                ..Default::default()
            };
            if let Some(timeout) = self.replay_install_wait(wait, REPLAY_SETTLE_TIMEOUT) {
                return timeout;
            }
        }
        let Some(step) = self.replay.as_mut().and_then(|plan| plan.next_step()) else {
            if !self.replay_finished {
                self.replay_finished = true;
                let wait = crate::app::browser::ReplayWait {
                    needs_engine: true,
                    engine_grace_until: Some(Instant::now() + REPLAY_FINAL_GRACE),
                    ..Default::default()
                };
                if let Some(timeout) = self.replay_install_wait(wait, REPLAY_SETTLE_TIMEOUT) {
                    return timeout;
                }
                return Task::done(Message::ReplayStep);
            }
            self.replay = None;
            self.replay_finished = false;
            self.replay_wait = None;
            crate::platform::trace::manifest("replay.done", serde_json::json!({ "flow": flow }));
            // Automated sessions must not leave the browser on screen after
            // the plan finishes. The trace is fully written before close.
            return self.close_browser_window();
        };
        if step.app == "browser" {
            let state = trace_browser_state(&self.browser.panel);
            let checks = crate::platform::trace::replay::check_requires(&step.requires, &state);
            if checks
                .iter()
                .any(|check| check.get("pass").and_then(serde_json::Value::as_bool) != Some(true))
            {
                // Best-effort replay: the target URL/index resolver below can
                // still land on the recorded entry when stack positions moved.
                crate::platform::trace::event(
                    crate::platform::trace::Lane::Platform,
                    "replay.mismatch",
                )
                .field("flow", flow)
                .field("app", &step.app)
                .field("action", &step.action)
                .field("checks", checks)
                .field("pre", &step.pre)
                .emit();
            }
        }
        crate::platform::trace::replay::set_replaying(true);
        let frame_before = self.browser.panel.session.frame_slot().generation();
        let title_before = self.browser.panel.page.title_generation;
        let task = self.replay_dispatch(&step);
        crate::platform::trace::replay::set_replaying(false);
        match self.replay_begin_settle(&step, frame_before, title_before) {
            Some(timeout) => Task::batch([task, timeout]),
            // Nothing to observe (e.g. Circle already closed the Browser):
            // continue immediately instead of waiting for a redraw that may
            // never exist.
            None => Task::batch([task, Task::done(Message::ReplayStep)]),
        }
    }

    /// Install a settle gate. Returns its timeout task, or `None` when the
    /// condition is already satisfied (the caller's `replay_kick` continues).
    fn replay_install_wait(
        &mut self,
        mut wait: crate::app::browser::ReplayWait,
        timeout: std::time::Duration,
    ) -> Option<Task<Message>> {
        if self.replay_wait_ready_for(&wait) {
            return None;
        }
        self.replay_wait_seq = self.replay_wait_seq.wrapping_add(1);
        wait.id = self.replay_wait_seq;
        let id = wait.id;
        let grace = wait.engine_grace_until;
        self.replay_wait = Some(wait);
        let mut tasks = vec![Task::perform(super::delay(timeout), move |_| {
            Message::ReplayWaitTimeout(id)
        })];
        if let Some(grace) = grace {
            let after = grace.saturating_duration_since(Instant::now());
            tasks.push(Task::perform(super::delay(after), move |_| {
                Message::ReplayWaitGrace(id)
            }));
        }
        Some(Task::batch(tasks))
    }

    /// Classify the step just dispatched and install its settle gate.
    fn replay_begin_settle(
        &mut self,
        step: &crate::platform::trace::replay::ReplayStep,
        frame_before: u64,
        title_before: u64,
    ) -> Option<Task<Message>> {
        use crate::app::browser::ReplayWait;
        let now = Instant::now();
        let action = step.action.as_str();
        let button = step
            .params
            .get("button")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default();
        let mut wait = ReplayWait::default();
        match action {
            // Navigation/history actions already carry a pending settle guard
            // set synchronously by the dispatcher.
            "navigate" | "history.back" | "history.forward" | "history.open" | "history.pick"
            | "history.cancel" => {
                wait.needs_engine = true;
                // Wait for the new document's first paint (not just the load
                // event) so a following screenshot cannot read the old slot.
                wait.needs_frame = true;
                wait.frame_mark = frame_before;
            }
            "grid.open" | "grid.close" | "grid.activate" | "grid.new_tab" | "grid.close_tab"
            | "grid.bookmark" | "grid.focus" => {
                wait.needs_redraw = true;
                wait.needs_grid_idle = true;
            }
            "pad.tap" if button == "circle" && self.browser_visible => {
                // Page -> tabs, or tabs -> Home. Wait for the grid FLIP and
                // the shell mode switch respectively.
                wait.needs_redraw = true;
                wait.needs_grid_idle = true;
                wait.needs_shell_idle = true;
            }
            "screen.ps" | "screen.home" => {
                // Resume opens the Browser asynchronously; dismiss has no
                // window to wait for.
                if self.browser_visible {
                    wait.needs_browser_ready = true;
                    wait.needs_engine = true;
                    wait.needs_redraw = true;
                }
                // Rapid replay steps must not stack PS/mode transitions on
                // top of each other; a real user sees the card settle first.
                wait.needs_shell_idle = true;
            }
            // Screenshots complete when their capture message lands.
            "screenshot" | "osk.screenshot" => {
                wait.needs_capture = true;
                wait.captures_to_finish = self.replay_capture_pending;
                if wait.captures_to_finish == 0 {
                    wait.needs_capture = false;
                }
                if action == "osk.screenshot" && self.osk.state.open {
                    wait.needs_osk_ready = true;
                }
                if action == "screenshot" {
                    wait.needs_current_frame = true;
                }
            }
            // The dock action needs the post-navigation A11y set to resolve.
            "cursor.dock" => {
                wait.needs_redraw = true;
                wait.needs_focusables = true;
            }
            "wait" => {
                wait.needs_engine = true;
                wait.needs_redraw = true;
                if self.osk.state.open {
                    wait.needs_osk_ready = true;
                }
            }
            "key" => {
                wait.needs_redraw = true;
                let enter = step
                    .params
                    .get("key")
                    .and_then(serde_json::Value::as_str)
                    .is_some_and(|key| {
                        key.eq_ignore_ascii_case("enter") || key.eq_ignore_ascii_case("return")
                    });
                if enter && self.browser_visible {
                    // Enter can start a page navigation; let ActivityChanged
                    // report it before the next step reads app state.
                    wait.needs_engine = true;
                    wait.engine_grace_until = Some(now + REPLAY_NAV_GRACE);
                }
            }
            "mouse.click" | "pad.tap" if self.browser_visible => {
                wait.needs_redraw = true;
                wait.needs_engine = true;
                wait.engine_grace_until = Some(now + REPLAY_NAV_GRACE);
                if self.browser.panel.grid.is_grid() || button == "create" {
                    wait.needs_grid_idle = true;
                }
                if self.osk.state.open {
                    wait.needs_osk_ready = true;
                }
            }
            "osk.open" => {
                wait.needs_redraw = true;
                wait.needs_osk_ready = true;
            }
            // OSK input uses the shared D-pad replay path; there is no
            // split-stick/trigger surface to synthesize.
            "pad.tap" if step.app == "browser" && button == "circle" => {
                // Circle closed the Browser: there may be no window left to
                // present. Finish this step immediately.
            }
            "mouse.drag" | "mouse.wheel" => {
                wait.needs_redraw = true;
                // Selection/scroll changes must reach the page paint before
                // the next synthetic input reads the new viewport state.
                wait.needs_frame = true;
                wait.frame_mark = frame_before;
                wait.needs_title = true;
                wait.title_mark = title_before;
            }
            "text" | "ime.preedit" | "ime.commit" | "pad.stick" | "osk.close" => {
                wait.needs_redraw = true;
            }
            "pad.dir" => {
                wait.needs_redraw = true;
                if self.osk.state.open {
                    // The key follows the same D-pad tick inside the pull
                    // model; wait for the queued synthetic frames to drain.
                    wait.needs_osk_ready = true;
                    wait.needs_osk_idle = true;
                } else {
                    wait.needs_cursor_idle = true;
                    // A nudge ends with a DOM reseed around the landing point.
                    // The next direction reads the freshly culled target set,
                    // so wait for that tree instead of the pre-jump one. A
                    // no-op press leaves the cursor in place and needs no
                    // reseed.
                    if self.browser.panel.cursor.snapshot().fly_to.is_some() {
                        wait.needs_focusables = true;
                        wait.focusables_after = Some(self.browser.panel.page.focusables_generation);
                    }
                }
            }
            _ => {
                wait.needs_redraw = true;
            }
        }
        // Launcher/manual-form steps are app-state changes; their next step
        // reads that state directly. Don't wait for a present that may not
        // exist (closing the only window) unless the step is visual.
        if step.app == "launcher" && !matches!(action, "wait" | "screenshot" | "osk.screenshot") {
            wait.needs_redraw = false;
        }
        self.replay_install_wait(wait, REPLAY_SETTLE_TIMEOUT)
    }

    /// True when the active settle gate's signals have all landed.
    fn replay_wait_ready(&self) -> bool {
        self.replay_wait
            .as_ref()
            .is_some_and(|wait| self.replay_wait_ready_for(wait))
    }

    fn replay_wait_ready_for(&self, wait: &crate::app::browser::ReplayWait) -> bool {
        if wait.needs_browser_ready
            && !(self.browser_visible
                && self.browser_window.is_some()
                && (self.browser_window_size.is_some() || self.browser.panel.session.has_frame()))
        {
            return false;
        }
        if wait.needs_engine {
            if wait
                .engine_grace_until
                .is_some_and(|grace| Instant::now() < grace)
            {
                return false;
            }
            if self.pending_browser_trace.is_some() || self.pending_history_travel.is_some() {
                return false;
            }
            if self.browser.panel.page.activity != crate::browser::engine::EngineActivity::Idle {
                return false;
            }
        }
        if wait.needs_focusables {
            let current_url = self.browser.panel.session.url();
            if self.browser.panel.page.focusables.is_empty()
                || self.browser.panel.page.focusables_url.as_deref() != Some(current_url)
                || wait
                    .focusables_after
                    .is_some_and(|mark| self.browser.panel.page.focusables_generation <= mark)
            {
                return false;
            }
        }
        if wait.needs_frame {
            let current = self.browser.panel.session.url();
            let slot = self.browser.panel.session.frame_slot();
            if slot.generation() <= wait.frame_mark || slot.url().as_deref() != Some(current) {
                return false;
            }
        }
        if wait.needs_current_frame {
            let current = self.browser.panel.session.url();
            if self.browser.panel.session.frame_slot().url().as_deref() != Some(current) {
                return false;
            }
        }
        if wait.needs_title && self.browser.panel.page.title_generation <= wait.title_mark {
            return false;
        }
        if wait.needs_osk_ready
            && !(self.osk.state.open && self.osk.window.is_some() && !self.osk.pending_show)
        {
            return false;
        }
        if wait.needs_osk_idle && self.osk.slot.has_replay() {
            return false;
        }
        if wait.needs_cursor_idle && self.browser.panel.cursor.snapshot().fly_to.is_some() {
            return false;
        }
        if wait.needs_grid_idle && self.browser.panel.grid.animating() {
            return false;
        }
        if wait.needs_shell_idle && !self.shell_idle() {
            return false;
        }
        if wait.needs_redraw && !wait.redraw_seen {
            return false;
        }
        if wait.needs_capture
            && wait.captures_to_finish > 0
            && self.replay_capture_pending >= wait.captures_to_finish
        {
            return false;
        }
        true
    }

    /// Schedule the next step if the current gate is satisfied. Clears the
    /// gate here so several events arriving before the queued `ReplayStep`
    /// cannot queue duplicate step ticks.
    pub(crate) fn replay_kick(&mut self) -> Task<Message> {
        if self.replay.is_none() || self.replay_wait.is_none() {
            return Task::none();
        }
        if self.replay_wait_ready() {
            self.replay_wait = None;
            Task::done(Message::ReplayStep)
        } else {
            Task::none()
        }
    }

    pub(crate) fn replay_note_frame(&mut self) {
        if let Some(wait) = &mut self.replay_wait {
            wait.redraw_seen = true;
        }
    }

    /// Redraw subscription is only alive while a gate needs a present (or the
    /// OSK surface is draining queued replay frames and needs wake-ups).
    pub(crate) fn replay_wants_frame(&self) -> bool {
        self.replay_wait.as_ref().is_some_and(|wait| {
            (wait.needs_redraw && !wait.redraw_seen)
                || (wait.needs_osk_idle && self.osk.slot.has_replay())
                || (wait.needs_cursor_idle && self.browser.panel.cursor.snapshot().fly_to.is_some())
        })
    }

    pub(crate) fn on_replay_wait_timeout(&mut self, id: u64) -> Task<Message> {
        let Some(wait) = &self.replay_wait else {
            return Task::none();
        };
        if wait.id != id {
            return Task::none();
        }
        let needs_engine = wait.needs_engine;
        let needs_redraw = wait.needs_redraw;
        let redraw_seen = wait.redraw_seen;
        let needs_capture = wait.needs_capture;
        crate::platform::trace::event(
            crate::platform::trace::Lane::Platform,
            "replay.settle_timeout",
        )
        .field("id", id)
        .field("needs_engine", needs_engine)
        .field("needs_redraw", needs_redraw)
        .field("redraw_seen", redraw_seen)
        .field("needs_capture", needs_capture)
        .field("pending_trace", self.pending_browser_trace.is_some())
        .field("pending_travel", self.pending_history_travel.is_some())
        .field(
            "activity",
            format!("{:?}", self.browser.panel.page.activity),
        )
        .emit();
        self.replay_wait = None;
        Task::done(Message::ReplayStep)
    }

    /// The navigation debounce elapsed. Re-check the gate: a load that started
    /// in the meantime keeps it closed (pending/activity), otherwise the step
    /// can advance without waiting for the liveness timeout.
    pub(crate) fn on_replay_wait_grace(&mut self, id: u64) -> Task<Message> {
        if self.replay_wait.as_ref().is_some_and(|wait| wait.id == id) {
            Task::done(Message::ReplayStep)
        } else {
            Task::none()
        }
    }

    /// A screenshot finished writing; the capture gate can close now. The app
    /// update chain then wakes the replay driver.
    pub(crate) fn on_replay_capture_done(&mut self) -> Task<Message> {
        self.replay_capture_pending = self.replay_capture_pending.saturating_sub(1);
        Task::none()
    }

    fn replay_dispatch(
        &mut self,
        step: &crate::platform::trace::replay::ReplayStep,
    ) -> Task<Message> {
        #[cfg(debug_assertions)]
        if step.app == "launcher" {
            return self.replay_launcher_action(step);
        }
        if step.app != "browser" {
            crate::platform::trace::event(
                crate::platform::trace::Lane::Platform,
                "replay.unsupported",
            )
            .field("app", &step.app)
            .field("action", &step.action)
            .emit();
            return Task::none();
        }
        match step.action.as_str() {
            "navigate" => {
                let Some(url) = step.params.get("url").and_then(serde_json::Value::as_str) else {
                    return Task::none();
                };
                self.navigate_browser_to(url.to_string())
            }
            #[cfg(debug_assertions)]
            "tab.new" => {
                let url = step.params.get("url").and_then(serde_json::Value::as_str);
                self.open_browser_tab(url.map(str::to_string))
            }
            #[cfg(debug_assertions)]
            "grid.open" => self.open_browser_grid(),
            #[cfg(debug_assertions)]
            "screen.ps" => self.ps_screen_toggle(),
            #[cfg(debug_assertions)]
            "screen.home" => self.return_to_home_screen(),
            #[cfg(debug_assertions)]
            "grid.close" => self.close_browser_grid(None),
            #[cfg(debug_assertions)]
            "grid.activate" => {
                let Some(index) = step.params.get("index").and_then(serde_json::Value::as_u64)
                else {
                    return Task::none();
                };
                let tabs = crate::ui::browser::grid::grid_tabs(
                    &self.browser.panel,
                    &self.browser.panel.grid.visual,
                );
                let Some(tab) = tabs.get(index as usize) else {
                    return Task::none();
                };
                self.browser_grid_select(tab.id)
            }
            #[cfg(debug_assertions)]
            "grid.new_tab" => {
                let url = step.params.get("url").and_then(serde_json::Value::as_str);
                if let Some(url) = url {
                    let open = self.open_browser_tab(Some(url.to_string()));
                    let id = self.browser.panel.active_tab;
                    // Opening the tab already cues; the close FLIP is silent.
                    let close = match self.close_browser_grid_with(
                        Some(id),
                        Some(crate::ui::browser::grid::GridFocus::Tab(id)),
                    ) {
                        GridCloseOutcome::Started(task) => task,
                        GridCloseOutcome::Rejected => Task::none(),
                    };
                    open.chain(close)
                } else {
                    self.browser_grid_open_plus()
                }
            }
            #[cfg(debug_assertions)]
            "grid.close_tab" => {
                let Some(index) = step.params.get("index").and_then(serde_json::Value::as_u64)
                else {
                    return Task::none();
                };
                let tabs = crate::ui::browser::grid::grid_tabs(
                    &self.browser.panel,
                    &self.browser.panel.grid.visual,
                );
                let Some(tab) = tabs.get(index as usize) else {
                    return Task::none();
                };
                self.browser_grid_close_tab(tab.id)
            }
            #[cfg(debug_assertions)]
            "grid.bookmark" => {
                let index = step
                    .params
                    .get("index")
                    .and_then(serde_json::Value::as_u64)
                    .unwrap_or(0);
                let tabs = crate::ui::browser::grid::grid_tabs(
                    &self.browser.panel,
                    &self.browser.panel.grid.visual,
                );
                let Some(tab) = tabs.get(index as usize) else {
                    return Task::none();
                };
                self.browser_grid_toggle_bookmark(tab.id)
            }
            #[cfg(debug_assertions)]
            "grid.focus" => {
                let dir = match step.params.get("dir").and_then(serde_json::Value::as_str) {
                    Some("up") => crate::browser::cursor::JumpDir::Up,
                    Some("down") => crate::browser::cursor::JumpDir::Down,
                    Some("left") => crate::browser::cursor::JumpDir::Left,
                    Some("right") => crate::browser::cursor::JumpDir::Right,
                    _ => return Task::none(),
                };
                self.browser_grid_move_focus(dir)
            }
            "click" => {
                let x = step.params.get("x").and_then(serde_json::Value::as_f64);
                let y = step.params.get("y").and_then(serde_json::Value::as_f64);
                let (Some(x), Some(y)) = (x, y) else {
                    return Task::none();
                };
                let tab = self.browser_tab();
                crate::platform::trace::event(crate::platform::trace::Lane::Engine, "command")
                    .field("command", "ClickAt")
                    .field("tab", tab.0)
                    .field("x", x)
                    .field("y", y)
                    .field("trusted", true)
                    .emit();
                if let Some(engine) = &self.browser.engine {
                    engine.send(EngineCommand::ClickAt {
                        tab,
                        x: x as f32,
                        y: y as f32,
                        target: None,
                        trusted: true,
                    });
                }
                Task::none()
            }
            #[cfg(debug_assertions)]
            "cursor.dock" => {
                if let Some((x, y)) = replay_point(&step.params) {
                    let panel = &mut self.browser.panel;
                    let id = panel
                        .focusables
                        .iter()
                        .find(|f| x >= f.x && x <= f.x + f.w && y >= f.y && y <= f.y + f.h)
                        .map(|f| f.id);
                    let mut cursor = panel.cursor.lock();
                    cursor.cursor.attached = id;
                    cursor.cursor.x = x;
                    cursor.cursor.y = y;
                    cursor.cursor.dock_blend = if id.is_some() { 1.0 } else { 0.0 };
                    cursor.cursor.last_dock = id.and_then(|id| {
                        panel
                            .focusables
                            .iter()
                            .find(|f| f.id == id)
                            .map(|f| (f.x, f.y, f.w, f.h))
                    });
                }
                Task::none()
            }
            #[cfg(debug_assertions)]
            "pad.stick" => {
                let x = step
                    .params
                    .get("x")
                    .and_then(serde_json::Value::as_f64)
                    .unwrap_or(0.0) as f32;
                let y = step
                    .params
                    .get("y")
                    .and_then(serde_json::Value::as_f64)
                    .unwrap_or(0.0) as f32;
                let mut sample = crate::domain::pad::PadSample {
                    lstick_x: x,
                    lstick_y: y,
                    stick_x: x,
                    stick_y: y,
                    ..Default::default()
                };
                sample.rstick_x = step
                    .params
                    .get("rx")
                    .and_then(serde_json::Value::as_f64)
                    .unwrap_or(0.0) as f32;
                sample.rstick_y = step
                    .params
                    .get("ry")
                    .and_then(serde_json::Value::as_f64)
                    .unwrap_or(0.0) as f32;
                sample.r2 = step
                    .params
                    .get("r2")
                    .and_then(serde_json::Value::as_bool)
                    .unwrap_or(false);
                let pad = crate::domain::pad::PadId("replay".into());
                let reading = crate::ui::launcher::input::NavReading {
                    id: pad,
                    sample,
                    source: "replay",
                };
                let neutral = reading.sample.lstick_x == 0.0
                    && reading.sample.lstick_y == 0.0
                    && reading.sample.rstick_x == 0.0
                    && reading.sample.rstick_y == 0.0;
                self.input_hub
                    .set_replay_reading((!neutral).then(|| reading.clone()));
                self.input_hub
                    .publish(vec![reading], crate::domain::pad::SnapshotReason::Sample);
                self.invalidate_browser_window()
            }
            #[cfg(debug_assertions)]
            "pad.tap" => {
                let button = match step
                    .params
                    .get("button")
                    .and_then(serde_json::Value::as_str)
                {
                    Some("cross") => PadButton::Cross,
                    Some("circle") => PadButton::Circle,
                    Some("square") => PadButton::Square,
                    Some("triangle") => PadButton::Triangle,
                    Some("l3") => PadButton::L3,
                    Some("r3") => PadButton::R3,
                    Some("l1") => PadButton::L1,
                    Some("r1") => PadButton::R1,
                    Some("options") => PadButton::Options,
                    Some("create") | Some("share") => PadButton::Create,
                    _ => return Task::none(),
                };
                self.replay_pad_tap(button)
            }
            #[cfg(debug_assertions)]
            "pad.dir" => {
                let action = match step.params.get("dir").and_then(serde_json::Value::as_str) {
                    Some("up") => NavAction::Up,
                    Some("down") => NavAction::Down,
                    Some("left") => NavAction::Left,
                    Some("right") => NavAction::Right,
                    _ => return Task::none(),
                };
                self.replay_pad_dir(action)
            }
            // OSK input uses the shared D-pad replay path; there is no
            // split-stick/trigger surface to synthesize.
            #[cfg(debug_assertions)]
            "osk.open" => self.request_osk_open(crate::ui::osk::OskTarget::BrowserPage),
            #[cfg(debug_assertions)]
            "osk.close" => self.close_osk("replay"),
            "history.back" | "history.forward" => {
                let target_url = step.params.get("url").and_then(serde_json::Value::as_str);
                let resolved = target_url.and_then(|url| {
                    self.browser
                        .panel
                        .session
                        .nav_entries()
                        .iter()
                        .position(|entry| entry.url == url)
                });
                if let Some(index) = resolved {
                    // The recorded position moved; resolve by identity.
                    self.on_browser_history_pick(index)
                } else if step.action == "history.back" {
                    self.on_browser_go_back()
                } else {
                    self.on_browser_go_forward()
                }
            }
            #[cfg(debug_assertions)]
            "text" => {
                if let Some(text) = step.params.get("text").and_then(serde_json::Value::as_str) {
                    self.replay_type_text(text)
                } else {
                    Task::none()
                }
            }
            #[cfg(debug_assertions)]
            "key" => {
                let Some(key) = step.params.get("key").and_then(serde_json::Value::as_str) else {
                    return Task::none();
                };
                let modifiers = replay_modifiers(step.params.get("modifiers"));
                // Synthetic real keyboard: close the OSK first; Escape is
                // consumed, other keys still reach the page (hand-off). An
                // Escape that cancels URL editing is not a plain hand-off.
                let escape = matches!(key.to_ascii_lowercase().as_str(), "escape" | "esc");
                let reason = if escape && self.browser.panel.url_editing {
                    "url_cancel"
                } else {
                    "keyboard"
                };
                let close = self.close_osk(reason);
                if escape {
                    return close;
                }
                if self.launcher_state.manual_add.is_some() && !self.browser_visible {
                    if key.eq_ignore_ascii_case("enter") {
                        return close.chain(self.on_launcher_confirm());
                    }
                    if key.eq_ignore_ascii_case("escape") {
                        return close.chain(self.cancel_manual_add());
                    }
                }
                let task = if let Some(task) = self.replay_press_key(key, modifiers) {
                    task
                } else if key.chars().count() == 1 {
                    self.replay_type_text(key)
                } else {
                    crate::platform::trace::event(
                        crate::platform::trace::Lane::Platform,
                        "replay.unsupported",
                    )
                    .field("app", &step.app)
                    .field("action", &step.action)
                    .field("key", key)
                    .emit();
                    Task::none()
                };
                close.chain(task)
            }
            #[cfg(debug_assertions)]
            "wait" => {
                // A no-op replay step is also a settle pump: invalidate the
                // Browser so a just-committed navigation presents before the
                // next screenshot step.
                self.browser_window
                    .map(crate::ui::layout::invalidate_window)
                    .unwrap_or_else(Task::none)
            }
            #[cfg(debug_assertions)]
            "screenshot" => {
                self.browser.panel.debug_overlay = false;
                self.replay_capture_pending = self.replay_capture_pending.saturating_add(1);
                // Capture one settle-delayed frame. A screenshot requested in
                // the same update as a committed navigation can still read the
                // previous presented frame.
                let invalidate = self
                    .browser_window
                    .map(crate::ui::layout::invalidate_window)
                    .unwrap_or_else(Task::none);
                invalidate.chain(Task::perform(
                    super::delay(std::time::Duration::from_millis(250)),
                    |()| Message::Browser(crate::ui::browser::BrowserMessage::Capture),
                ))
            }
            #[cfg(debug_assertions)]
            "osk.screenshot" => {
                // Same settle delay as the Browser shot. The OSK is a separate
                // window: capture it directly so transparent regions and both
                // keyboard halves are visible to pixel QA.
                self.replay_capture_pending = self.replay_capture_pending.saturating_add(1);
                let invalidate = self
                    .osk
                    .window
                    .map(crate::ui::layout::invalidate_window)
                    .unwrap_or_else(Task::none);
                invalidate.chain(Task::perform(
                    super::delay(std::time::Duration::from_millis(250)),
                    |()| Message::OskCapture,
                ))
            }
            #[cfg(debug_assertions)]
            "ime.preedit" => {
                let close = self.close_osk("ime");
                if let Some(text) = step.params.get("text").and_then(serde_json::Value::as_str)
                    && let Some(id) = self.browser_window
                {
                    let task = self.on_browser_ime(
                        id,
                        iced::advanced::input_method::Event::Preedit(text.to_string(), None),
                    );
                    return close.chain(task);
                }
                close
            }
            #[cfg(debug_assertions)]
            "ime.commit" => {
                let close = self.close_osk("ime");
                if let Some(text) = step.params.get("text").and_then(serde_json::Value::as_str)
                    && let Some(id) = self.browser_window
                {
                    let task = self.on_browser_ime(
                        id,
                        iced::advanced::input_method::Event::Commit(text.to_string()),
                    );
                    return close.chain(task);
                }
                close
            }
            #[cfg(debug_assertions)]
            "mouse.click" => {
                if self.osk.state.open {
                    // Real mouse input dismisses the controller-only OSK; the
                    // click does not need to reach the page.
                    return self.close_osk("mouse");
                }
                if let Some((x, y)) = replay_point(&step.params) {
                    let count = step
                        .params
                        .get("count")
                        .and_then(serde_json::Value::as_u64)
                        .unwrap_or(1)
                        .clamp(1, 3);
                    for _ in 0..count {
                        self.inject_browser_click(x, y, "replay");
                    }
                }
                Task::none()
            }
            #[cfg(debug_assertions)]
            "mouse.drag" => {
                if self.osk.state.open {
                    return self.close_osk("mouse");
                }
                let x1 = step.params.get("x1").and_then(serde_json::Value::as_f64);
                let y1 = step.params.get("y1").and_then(serde_json::Value::as_f64);
                let x2 = step.params.get("x2").and_then(serde_json::Value::as_f64);
                let y2 = step.params.get("y2").and_then(serde_json::Value::as_f64);
                if let (Some(x1), Some(y1), Some(x2), Some(y2)) = (x1, y1, x2, y2) {
                    self.inject_browser_drag(x1 as f32, y1 as f32, x2 as f32, y2 as f32, "replay");
                }
                Task::none()
            }
            #[cfg(debug_assertions)]
            "mouse.wheel" => {
                if self.osk.state.open {
                    return self.close_osk("mouse");
                }
                if let Some((x, y)) = replay_point(&step.params) {
                    let dx = step
                        .params
                        .get("dx")
                        .and_then(serde_json::Value::as_i64)
                        .unwrap_or(0);
                    let dy = step
                        .params
                        .get("dy")
                        .and_then(serde_json::Value::as_i64)
                        .unwrap_or(0);
                    self.inject_browser_wheel(x, y, dx as i32, dy as i32, "replay");
                }
                Task::none()
            }
            _ => {
                crate::platform::trace::event(
                    crate::platform::trace::Lane::Platform,
                    "replay.unsupported",
                )
                .field("app", &step.app)
                .field("action", &step.action)
                .emit();
                Task::none()
            }
        }
    }

    /// Open a browser navigation/history action whose outcome is settled by a
    /// later engine event. The pre-state is captured now; the pending guard is
    /// closed by `settle_browser_action_trace` when canonical history lands.
    pub(crate) fn begin_browser_action_trace(
        &mut self,
        action: &'static str,
        source: &'static str,
        requested_index: Option<usize>,
        requested_url: Option<String>,
    ) -> Option<crate::platform::trace::FlowId> {
        // A new navigation/history intent supersedes any in-flight jump.
        self.pending_history_travel = None;
        if !crate::platform::trace::actions_enabled() {
            return None;
        }
        if let Some(old) = self.pending_browser_trace.take() {
            old.action
                .abandon("superseded")
                .post(trace_browser_state(&self.browser.panel))
                .emit();
        }
        let replaying = crate::platform::trace::replay::replaying();
        let source = if replaying { "replay" } else { source };
        let source_flow = if replaying {
            self.replay.as_ref().map(|plan| plan.flow())
        } else {
            None
        };
        let current = self.browser.panel.session.nav_current();
        let mut builder =
            crate::platform::trace::action(crate::platform::trace::Lane::Browser, action)
                .app("browser")
                .source(source)
                .field("replay", replaying)
                .field("source_flow", source_flow)
                .field("requested_index", requested_index)
                .field("requested_url", requested_url.as_deref())
                .pre(trace_browser_state(&self.browser.panel))
                .repro(serde_json::json!({
                    "app": "browser",
                    "action": action.strip_prefix("browser.").unwrap_or(action),
                    "params": {
                        "index": requested_index,
                        "url": requested_url.as_deref(),
                    },
                }));
        if let Some(current) = current {
            builder = builder.require("ui.nav.current", current);
        }
        let guard = builder.begin();
        let flow = guard.flow();
        let kind = if action.starts_with("browser.history.") {
            BrowserTraceKind::History
        } else {
            BrowserTraceKind::Navigate
        };
        self.pending_browser_trace = Some(crate::app::browser::PendingBrowserTrace {
            action: guard.detach(),
            kind,
            requested_index,
            requested_url,
            generation: None,
        });
        Some(flow)
    }

    /// Start a selector jump toward an absolute session index. The engine
    /// resolves the index to a DevTools entry id and calls
    /// `Page.navigateToHistoryEntry`, which bypasses Chromium's Back/Forward
    /// skippable-entry filter and keeps the forward tail.
    pub(crate) fn start_history_travel(
        &mut self,
        action: &'static str,
        source: &'static str,
        target_index: usize,
        target_url: Option<String>,
    ) {
        let Some(start_index) = self.browser.panel.session.nav_current() else {
            self.trace_browser_noop(action, source, "history_missing");
            return;
        };
        let start_url = self.browser.panel.session.url().to_string();
        self.begin_browser_action_trace(action, source, Some(target_index), target_url.clone());
        let tab = self.browser.panel.active_tab;
        self.pending_history_travel = Some(crate::app::browser::PendingHistoryTravel {
            tab,
            target_index: Some(target_index),
            target_url,
            start_index: Some(start_index),
            start_url,
            started: Instant::now(),
        });
        self.send_history_index(tab, target_index);
    }

    /// A single-step action is settled once both the canonical index and the
    /// live URL reflect the landing. CEF can report one before the other.
    fn single_step_landed(
        &self,
        current: Option<usize>,
        live_url: &str,
        start_index: Option<usize>,
        start_url: &str,
    ) -> bool {
        let Some(current) = current else {
            return false;
        };
        if Some(current) == start_index {
            return false;
        }
        if live_url != start_url {
            return true;
        }
        self.browser
            .panel
            .page
            .last_engine_history
            .as_ref()
            .and_then(|history| history.entries.get(current))
            .is_some_and(|entry| entry.url == live_url)
    }

    /// L1/R1: one CEF traversal step, no index target. Chromium may skip
    /// history-manipulation entries, so the landing index is whatever CEF
    /// reports; chasing `current ± 1` would oscillate.
    pub(crate) fn start_history_step(
        &mut self,
        action: &'static str,
        source: &'static str,
        back: bool,
    ) {
        let Some(start_index) = self.browser.panel.session.nav_current() else {
            self.trace_browser_noop(action, source, "history_missing");
            return;
        };
        let start_url = self.browser.panel.session.url().to_string();
        let flow = self.begin_browser_action_trace(action, source, None, None);
        let tab = self.browser.panel.active_tab;
        self.pending_history_travel = Some(crate::app::browser::PendingHistoryTravel {
            tab,
            target_index: None,
            target_url: None,
            start_index: Some(start_index),
            start_url,
            started: Instant::now(),
        });
        self.send_history_command(flow, tab, back);
    }

    /// Advance an in-flight history action after a canonical snapshot.
    pub(crate) fn advance_history_travel(&mut self, tab: crate::browser::engine::TabId) {
        let Some((pending_tab, target_index, target_url, start_index, start_url)) =
            self.pending_history_travel.as_ref().map(|pending| {
                (
                    pending.tab,
                    pending.target_index,
                    pending.target_url.clone(),
                    pending.start_index,
                    pending.start_url.clone(),
                )
            })
        else {
            return;
        };
        if pending_tab != tab {
            return;
        }
        let current = self.browser.panel.session.nav_current();
        let live_url = self.browser.panel.session.url();

        let Some(target_index) = target_index else {
            // Single CEF step: settle on any real movement, not a target.
            if self.single_step_landed(current, live_url, start_index, &start_url) {
                self.pending_history_travel = None;
                self.settle_browser_action_trace("settled");
            }
            return;
        };

        if current == Some(target_index) {
            // The snapshot can move the index before OnAddressChange updates
            // the live URL. Wait for the matching URL so the trace does not
            // settle on the previous page.
            let expected = target_url.or_else(|| {
                self.browser
                    .panel
                    .page
                    .last_engine_history
                    .as_ref()
                    .and_then(|history| history.entries.get(target_index))
                    .map(|entry| entry.url.clone())
            });
            if let Some(expected) = expected
                && live_url != expected
            {
                return;
            }
            self.pending_history_travel = None;
            self.settle_browser_action_trace("settled");
            return;
        }

        // An exact index jump either lands on the target or leaves the index
        // alone. A different landing means the session stack changed while
        // the CDP request was in flight (the resolved entry id no longer maps
        // to the picked row); load the requested URL explicitly rather than
        // chasing a stale index.
        if current != start_index {
            let fallback = target_url;
            self.pending_history_travel = None;
            self.settle_browser_action_trace("index_mismatch");
            if let Some(url) = fallback {
                self.history_url_fallback(url, "index_mismatch");
            }
        }
    }

    /// Close a history action when the address change confirms it.
    pub(crate) fn settle_history_travel_on_url(&mut self, tab: crate::browser::engine::TabId) {
        let Some((pending_tab, target_index, target_url, start_index, start_url)) =
            self.pending_history_travel.as_ref().map(|pending| {
                (
                    pending.tab,
                    pending.target_index,
                    pending.target_url.clone(),
                    pending.start_index,
                    pending.start_url.clone(),
                )
            })
        else {
            return;
        };
        if pending_tab != tab {
            return;
        }
        let current = self.browser.panel.session.nav_current();
        let live_url = self.browser.panel.session.url();
        if target_index.is_none() {
            if self.single_step_landed(current, live_url, start_index, &start_url) {
                self.pending_history_travel = None;
                self.settle_browser_action_trace("settled");
            }
            return;
        }
        if current != target_index {
            return;
        }
        if let Some(expected) = target_url
            && live_url != expected
        {
            return;
        }
        self.pending_history_travel = None;
        self.settle_browser_action_trace("settled");
    }

    /// Redirected history entries can land on a URL different from the
    /// recorded entry; load completion is the final authority once the index
    /// already matches.
    pub(crate) fn settle_history_travel_on_load(&mut self, tab: crate::browser::engine::TabId) {
        let Some((pending_tab, target_index, start_index, start_url)) =
            self.pending_history_travel.as_ref().map(|pending| {
                (
                    pending.tab,
                    pending.target_index,
                    pending.start_index,
                    pending.start_url.clone(),
                )
            })
        else {
            return;
        };
        if pending_tab != tab {
            return;
        }
        let current = self.browser.panel.session.nav_current();
        let matched = match target_index {
            Some(target) => current == Some(target),
            None => self.single_step_landed(
                current,
                self.browser.panel.session.url(),
                start_index,
                &start_url,
            ),
        };
        if matched {
            self.pending_history_travel = None;
            self.settle_browser_action_trace("settled");
        }
    }

    /// Send the exact index jump. The next canonical snapshot settles it.
    fn send_history_index(&mut self, tab: crate::browser::engine::TabId, target_index: usize) {
        let flow = self
            .pending_browser_trace
            .as_ref()
            .map(|pending| pending.action.flow());
        if let Some(flow) = flow {
            crate::platform::trace::event(crate::platform::trace::Lane::Engine, "command")
                .flow(flow)
                .field("command", "GoToHistoryIndex")
                .field("tab", tab.0)
                .field("index", target_index)
                .emit();
        }
        self.send_engine_traced(
            flow,
            EngineCommand::GoToHistoryIndex {
                tab,
                index: target_index,
            },
        );
    }

    /// Settle a selector jump whose DevTools round trip failed and load the
    /// requested URL as the last resort (a new history entry; forward tail
    /// lost, but the user's pick is honoured).
    pub(crate) fn on_history_jump_failed(
        &mut self,
        tab: crate::browser::engine::TabId,
        index: usize,
        reason: String,
    ) {
        let Some(pending) = self.pending_history_travel.as_ref() else {
            return;
        };
        if pending.tab != tab || pending.target_index != Some(index) {
            return;
        }
        let target_url = pending.target_url.clone();
        self.pending_history_travel = None;
        self.settle_browser_action_trace("jump_failed");
        if let Some(url) = target_url {
            crate::platform::trace::event(
                crate::platform::trace::Lane::Browser,
                "history.fallback",
            )
            .field("url", &url)
            .field("reason", "jump_failed")
            .field("engine_reason", reason)
            .emit();
            let _ = self.navigate_browser_to(url);
        }
    }

    /// Bound a history action that never reports a landing or an error.
    pub(crate) fn check_history_travel_timeout(&mut self) {
        let expired = self
            .pending_history_travel
            .as_ref()
            .is_some_and(|pending| pending.started.elapsed() >= HISTORY_TRAVEL_TIMEOUT);
        if !expired {
            return;
        }
        let fallback = self
            .pending_history_travel
            .as_ref()
            .and_then(|pending| pending.target_url.clone());
        self.pending_history_travel = None;
        self.settle_browser_action_trace("timeout");
        if let Some(url) = fallback {
            self.history_url_fallback(url, "timeout");
        }
    }

    fn history_url_fallback(&mut self, url: String, reason: &'static str) {
        crate::platform::trace::event(crate::platform::trace::Lane::Browser, "history.fallback")
            .field("url", &url)
            .field("reason", reason)
            .emit();
        let _ = self.navigate_browser_to(url);
    }

    fn send_history_command(
        &mut self,
        flow: Option<crate::platform::trace::FlowId>,
        tab: crate::browser::engine::TabId,
        back: bool,
    ) {
        if let Some(flow) = flow {
            crate::platform::trace::event(crate::platform::trace::Lane::Engine, "command")
                .flow(flow)
                .field("command", if back { "GoBack" } else { "GoForward" })
                .field("tab", tab.0)
                .field("steps", 1)
                .emit();
        }
        self.send_engine_traced(
            flow,
            if back {
                EngineCommand::GoBack { tab }
            } else {
                EngineCommand::GoForward { tab }
            },
        );
    }

    /// Cancel a multi-step jump. The trace is closed as abandoned so the
    /// timeline never shows a step that was not performed.
    pub(crate) fn cancel_history_travel(&mut self, outcome: &'static str) {
        if self.pending_history_travel.take().is_none() {
            return;
        }
        if let Some(pending) = self.pending_browser_trace.take()
            && pending.kind == BrowserTraceKind::History
        {
            pending
                .action
                .abandon(outcome)
                .post(trace_browser_state(&self.browser.panel))
                .emit();
        }
    }

    /// Send an engine command, preserving the trace flow when present.
    pub(crate) fn send_engine_traced(
        &self,
        flow: Option<crate::platform::trace::FlowId>,
        command: crate::browser::engine::EngineCommand,
    ) {
        let Some(engine) = &self.browser.engine else {
            return;
        };
        if let Some(flow) = flow {
            engine.send_traced(flow, command);
        } else {
            engine.send(command);
        }
    }

    /// Record a browser action that ended before reaching the engine.
    pub(crate) fn trace_browser_noop(
        &mut self,
        action: &'static str,
        source: &'static str,
        reason: &'static str,
    ) {
        if !crate::platform::trace::actions_enabled() {
            return;
        }
        crate::platform::trace::action(crate::platform::trace::Lane::Browser, action)
            .app("browser")
            .source(source)
            .pre(trace_browser_state(&self.browser.panel))
            .begin()
            .finish("noop")
            .field("reason", reason)
            .post(trace_browser_state(&self.browser.panel))
            .emit();
    }

    /// Close the pending browser action with the settled UI/engine state.
    pub(crate) fn settle_browser_action_trace(&mut self, outcome: &'static str) {
        let Some(pending) = self.pending_browser_trace.take() else {
            return;
        };
        let session = &self.browser.panel.session;
        let actual_index = session.nav_current();
        let actual_url = session.url().to_string();
        let index_ok = pending
            .requested_index
            .is_none_or(|want| actual_index == Some(want));
        let url_ok = pending
            .requested_url
            .as_deref()
            .is_none_or(|want| actual_url == want);
        let engine_agree = self
            .browser
            .panel
            .page
            .last_engine_history
            .as_ref()
            .is_some_and(|history| {
                Some(history.current) == actual_index
                    && history
                        .entries
                        .get(history.current)
                        .map(|entry| entry.url.as_str())
                        == Some(actual_url.as_str())
            });
        let matched_by =
            if pending.requested_index.is_some() && pending.requested_index == actual_index {
                "index"
            } else if pending.requested_url.as_deref() == Some(actual_url.as_str()) {
                "url"
            } else {
                "none"
            };
        pending
            .action
            .settle(outcome)
            .post(trace_browser_state(&self.browser.panel))
            .field("actual_index", actual_index)
            .field("actual_url", &actual_url)
            .check("requested_index", index_ok)
            .check("requested_url", url_ok)
            .check("ui_engine_agree", engine_agree)
            .resolution(serde_json::json!({
                "requested_index": pending.requested_index,
                "actual_index": actual_index,
                "requested_url": pending.requested_url,
                "actual_url": actual_url,
                "matched_by": matched_by,
            }))
            .emit();
    }

    pub(crate) fn toggle_browser_grid(&mut self) -> Task<Message> {
        if self.browser.panel.grid.animating() {
            return Task::none();
        }
        if self.browser.panel.grid.is_grid() {
            self.close_browser_grid(None)
        } else {
            self.open_browser_grid()
        }
    }

    pub(crate) fn open_browser_grid(&mut self) -> Task<Message> {
        use crate::ui::browser::grid::{GridFocus, GridMode};

        if self.browser.panel.grid.is_grid() {
            return Task::none();
        }
        let close_osk = if self.osk.state.open {
            self.close_osk("tabs")
        } else {
            Task::none()
        };
        self.browser.panel.cancel_url_edit();
        self.browser.panel.refresh_grid_visual();
        let tabs_count = self.browser.panel.grid.visual.len();
        let active = self.browser.panel.active_tab;
        let offset = self.reveal_grid_focus(GridFocus::Tab(active));
        let reduced =
            crate::ui::motion::reduced_motion_effective(self.session.prefs.reduced_motion);
        if reduced {
            let panel = &mut self.browser.panel;
            panel.grid.mode = GridMode::Grid;
            panel.grid.anim = None;
            panel.grid.bookmark_scroll_x = 0.0;
            self.browser.dpad_stepper.reset();
            self.browser.grid_nav.prepare_on_open(true);
            self.browser.pending_grid_move = None;
            self.browser.grid_scroll_pending = Some(offset);
            self.note_launcher_activity();
            self.play_ui_cue(UiSoundKind::Action);
            crate::platform::trace::event(
                crate::platform::trace::Lane::Browser,
                "browser.grid.open",
            )
            .field("tabs", tabs_count)
            .field("reduced_motion", true)
            .emit();
            return close_osk
                .chain(self.invalidate_browser_window())
                .chain(self.pending_grid_scroll_later())
                .chain(self.apply_pending_grid_scroll());
        }
        #[cfg(debug_assertions)]
        crate::ui::browser::grid::reset_grid_anim_frames();
        let panel = &mut self.browser.panel;
        let generation = panel.grid.next_generation();
        panel.grid.mode = GridMode::Entering;
        panel.grid.bookmark_scroll_x = 0.0;
        self.browser.pending_grid_move = None;
        self.browser.grid_scroll_pending = Some(offset);
        panel.grid.anim = Some(crate::ui::browser::grid::GridAnim {
            generation,
            started: Instant::now(),
            tab: active,
            entering: true,
            card: GridFocus::Tab(active),
        });
        self.browser.dpad_stepper.reset();
        self.browser.grid_nav.prepare_on_open(false);
        self.note_launcher_activity();
        self.play_ui_cue(UiSoundKind::Action);
        crate::platform::trace::event(crate::platform::trace::Lane::Browser, "browser.grid.open")
            .field("tabs", tabs_count)
            .emit();
        let done = Task::perform(
            super::delay(std::time::Duration::from_millis(
                crate::ui::browser::grid::GRID_ANIM_MS,
            )),
            move |_| Message::Browser(crate::ui::browser::BrowserMessage::GridAnimDone(generation)),
        );
        close_osk
            .chain(self.invalidate_browser_window())
            .chain(self.pending_grid_scroll_later())
            .chain(done)
    }

    pub(crate) fn close_browser_grid(
        &mut self,
        select: Option<crate::browser::engine::TabId>,
    ) -> Task<Message> {
        match self.close_browser_grid_with(select, None) {
            GridCloseOutcome::Started(task) => {
                self.play_ui_cue(UiSoundKind::Nav);
                task
            }
            GridCloseOutcome::Rejected => Task::none(),
        }
    }

    /// Close the tabs view. Silent: the caller owns the cue. Callers whose
    /// initiating action already cued (tab open/bookmark open) use the task
    /// without emitting another.
    pub(crate) fn close_browser_grid_with(
        &mut self,
        select: Option<crate::browser::engine::TabId>,
        source: Option<crate::ui::browser::grid::GridFocus>,
    ) -> GridCloseOutcome {
        use crate::ui::browser::grid::{GridFocus, GridMode};

        if self.browser.panel.grid.animating() || !self.browser.panel.grid.is_grid() {
            return GridCloseOutcome::Rejected;
        }
        let select = select.filter(|id| self.browser.panel.tab_page(*id).is_some());
        let reduced =
            crate::ui::motion::reduced_motion_effective(self.session.prefs.reduced_motion);
        if reduced {
            let pending;
            {
                let panel = &mut self.browser.panel;
                panel.grid.mode = GridMode::Page;
                panel.grid.anim = None;
                pending = select.filter(|id| *id != panel.active_tab);
            }
            crate::platform::trace::event(
                crate::platform::trace::Lane::Browser,
                "browser.grid.close",
            )
            .field("select", select.is_some())
            .field("reduced_motion", true)
            .emit();
            if let Some(id) = pending {
                return GridCloseOutcome::Started(self.activate_browser_tab(id));
            }
            self.sync_live_mouse();
            return GridCloseOutcome::Started(self.invalidate_browser_window());
        }
        let tab = select.unwrap_or(self.browser.panel.active_tab);
        let source_focus = source
            .or_else(|| select.map(GridFocus::Tab))
            .unwrap_or(GridFocus::Tab(tab));
        // Start the FLIP from a visible card even when the debug action
        // selected an off-screen index.
        let offset = self.reveal_grid_focus(source_focus);
        let selected = select.filter(|id| *id != self.browser.panel.active_tab);
        // Activate the selected tab as the leaving FLIP starts, not when it
        // lands: the header curtain is already sliding in during the FLIP and
        // must show the selected tab's URL immediately.
        let activation = match selected {
            Some(id) => self.activate_browser_tab(id),
            None => Task::none(),
        };
        let panel = &mut self.browser.panel;
        #[cfg(debug_assertions)]
        crate::ui::browser::grid::reset_grid_anim_frames();
        let generation = panel.grid.next_generation();
        panel.grid.mode = GridMode::Leaving;
        self.browser.grid_scroll_pending = Some(offset);
        panel.grid.anim = Some(crate::ui::browser::grid::GridAnim {
            generation,
            started: Instant::now(),
            tab,
            entering: false,
            card: source_focus,
        });
        crate::platform::trace::event(crate::platform::trace::Lane::Browser, "browser.grid.close")
            .field("tab", tab.0)
            .field("select", select.is_some())
            .emit();
        let done = Task::perform(
            super::delay(std::time::Duration::from_millis(
                crate::ui::browser::grid::GRID_ANIM_MS,
            )),
            move |_| Message::Browser(crate::ui::browser::BrowserMessage::GridAnimDone(generation)),
        );
        GridCloseOutcome::Started(
            activation
                .chain(self.invalidate_browser_window())
                .chain(self.pending_grid_scroll_later())
                .chain(done),
        )
    }

    pub(crate) fn finish_browser_grid_transition(&mut self, generation: u64) -> Task<Message> {
        use crate::ui::browser::grid::GridMode;

        if self.browser.panel.grid.anim_generation != generation {
            return Task::none();
        }
        let mode = self.browser.panel.grid.mode;
        #[cfg(debug_assertions)]
        let anim = self.browser.panel.grid.anim;
        self.browser.panel.grid.anim = None;
        #[cfg(debug_assertions)]
        if let Some(anim) = anim {
            let (frames, max_gap_ms) = crate::ui::browser::grid::take_grid_anim_frames();
            crate::controller::hid::diag::diag_info(format!(
                "ui-diag: tabs anim frames={frames} elapsed_ms={} max_gap_ms={max_gap_ms} dir={mode:?}",
                anim.started.elapsed().as_millis()
            ));
        }
        match mode {
            GridMode::Entering => {
                self.browser.panel.grid.mode = GridMode::Grid;
                self.browser.panel.sync_grid_visual();
                // Arm immediately: the grid is visible now, and waiting for a
                // neutral sample swallowed the first D-pad/stick move.
                self.browser.grid_nav.prepare_on_open(true);
                crate::controller::hid::diag::diag_info("ui-diag: browser grid entered");
                let scroll = self.apply_pending_grid_scroll();
                if let Some((dir, pad)) = self.browser.pending_grid_move.take() {
                    self.set_haptic_pad_from_id(Some(&pad));
                    self.play_ui_cue(UiSoundKind::Nav);
                    self.haptic_pad_serial = None;
                    return scroll.chain(self.browser_grid_move_focus(dir));
                }
                scroll.chain(self.invalidate_browser_window())
            }
            GridMode::Leaving => {
                self.browser.panel.grid.mode = GridMode::Page;
                self.browser.dpad_stepper.reset();
                self.browser.grid_nav.reset();
                self.browser.pending_grid_move = None;
                self.browser.grid_scroll_pending = None;
                self.sync_live_mouse();
                self.invalidate_browser_window()
            }
            _ => Task::none(),
        }
    }

    /// Settle a transition when the Browser surface is closing: keep the grid
    /// if it was opening/open, return to the page if it was leaving.
    pub(crate) fn settle_browser_grid_for_close(&mut self) {
        let panel = &mut self.browser.panel;
        panel.grid.anim = None;
        self.browser.dpad_stepper.reset();
        self.browser.grid_nav.reset();
        self.browser.pending_grid_move = None;
        self.browser.grid_scroll_pending = None;
        match panel.grid.mode {
            crate::ui::browser::grid::GridMode::Entering => {
                panel.grid.mode = crate::ui::browser::grid::GridMode::Grid;
            }
            crate::ui::browser::grid::GridMode::Leaving => {
                panel.grid.mode = crate::ui::browser::grid::GridMode::Page;
            }
            _ => {}
        }
    }

    pub(crate) fn browser_grid_select(
        &mut self,
        id: crate::browser::engine::TabId,
    ) -> Task<Message> {
        use crate::ui::browser::grid::GridFocus;
        if self.browser.panel.grid.animating() || !self.browser.panel.grid.is_grid() {
            return Task::none();
        }
        if self.browser.panel.tab_page(id).is_none() {
            return Task::none();
        }
        self.browser.panel.grid.focus = GridFocus::Tab(id);
        match self.close_browser_grid_with(Some(id), Some(GridFocus::Tab(id))) {
            GridCloseOutcome::Started(task) => {
                self.play_ui_cue(UiSoundKind::Nav);
                task
            }
            GridCloseOutcome::Rejected => Task::none(),
        }
    }

    pub(crate) fn browser_grid_open_plus(&mut self) -> Task<Message> {
        use crate::ui::browser::grid::GridFocus;
        if self.browser.panel.grid.animating() || !self.browser.panel.grid.is_grid() {
            return Task::none();
        }
        if self.browser.panel.order.len() >= crate::browser::tabs::MAX_SAVED_TABS {
            return Task::none();
        }
        let open = self.open_browser_tab(None);
        let id = self.browser.panel.active_tab;
        // `open_browser_tab` already plays the tab-open cue.
        let close = match self.close_browser_grid_with(Some(id), Some(GridFocus::Plus)) {
            GridCloseOutcome::Started(task) => task,
            GridCloseOutcome::Rejected => Task::none(),
        };
        open.chain(close)
    }

    /// The one cue a tab close owns. `ResetDefault` navigates silently, so
    /// this is the only cue the final-tab path emits.
    pub(crate) fn close_tab_cue(
        outcome: crate::ui::browser::state::TabCloseOutcome,
    ) -> Option<UiSoundKind> {
        use crate::ui::browser::state::TabCloseOutcome;
        match outcome {
            TabCloseOutcome::Closed { .. } => Some(UiSoundKind::Action),
            TabCloseOutcome::ResetDefault => Some(UiSoundKind::Nav),
            TabCloseOutcome::Rejected => None,
        }
    }

    pub(crate) fn browser_grid_close_tab(
        &mut self,
        id: crate::browser::engine::TabId,
    ) -> Task<Message> {
        use crate::ui::browser::grid::GridFocus;
        if self.browser.panel.grid.animating() || !self.browser.panel.grid.is_grid() {
            return Task::none();
        }
        if self.browser.panel.tab_page(id).is_none() {
            return Task::none();
        }
        let focus = self.browser.panel.grid.focus;
        let next_focus =
            crate::ui::browser::grid::focus_after_close(&self.browser.panel.grid.visual, focus, id);
        // The close is silent; the outcome owns the single cue.
        let outcome = self.close_browser_tab(id);
        if let Some(kind) = Self::close_tab_cue(outcome) {
            self.play_ui_cue(kind);
        }
        let panel = &mut self.browser.panel;
        panel.sync_grid_visual();
        panel.grid.focus = next_focus;
        if !matches!(next_focus, GridFocus::Bookmark(_)) {
            panel.grid.row_focus = next_focus;
        }
        let scroll = match next_focus {
            GridFocus::Bookmark(_) => Task::none(),
            _ => self.scroll_grid_focus_into_view(),
        };
        scroll.chain(self.invalidate_browser_window())
    }

    /// Persist the focused tabs-view card as the active tab when the screen
    /// closes (Circle -> Home). Selecting a card is a grid focus change until
    /// the user enters it; leaving must keep that choice.
    pub(crate) fn persist_grid_selection(&mut self) -> Task<Message> {
        use crate::ui::browser::grid::GridFocus;
        if !self.browser.panel.grid.is_grid() {
            return Task::none();
        }
        let focus = self.browser.panel.grid.focus;
        let task = match focus {
            GridFocus::Tab(id) => self.activate_browser_tab(id),
            _ => Task::none(),
        };
        self.save_page();
        task
    }

    /// Stage size the Browser card is laid out in. The cover is set before the
    /// first frame; the size event only confirms it.
    fn browser_stage(&self) -> (f32, f32) {
        self.browser_window_size
            .or_else(|| {
                self.browser_monitor_cover
                    .map(|cover| (cover.width, cover.height))
            })
            .unwrap_or((1920.0, 1080.0))
    }

    fn browser_tabs_geometry(&self) -> crate::ui::browser::grid::TabsCardGeometry {
        let (stage_w, stage_h) = self.browser_stage();
        crate::ui::browser::grid::tabs_card_geometry(stage_w, stage_h)
    }

    fn browser_grid_focus_count(&self) -> usize {
        self.browser.panel.grid.visual.len() + 1
    }

    /// Reveal `focus` in the horizontal strip and return the new offset.
    fn reveal_grid_focus(&mut self, focus: crate::ui::browser::grid::GridFocus) -> f32 {
        use crate::ui::browser::grid::GridFocus;
        let geometry = self.browser_tabs_geometry();
        let slot = match focus {
            GridFocus::Plus | GridFocus::Bookmark(_) => 0,
            GridFocus::Tab(id) => self
                .browser
                .panel
                .grid
                .visual
                .iter()
                .position(|tab| *tab == id)
                .map(|index| index + 1)
                .unwrap_or(0),
        };
        let offset = crate::ui::browser::grid::reveal_offset(
            &geometry.layout,
            slot,
            self.browser_grid_focus_count(),
            self.browser.panel.grid.tab_scroll_x,
        );
        self.browser.panel.grid.tab_scroll_x = offset;
        offset
    }

    /// Keep the focused tab card inside the horizontal strip viewport.
    fn scroll_grid_focus_into_view(&mut self) -> Task<Message> {
        let focus = self.browser.panel.grid.focus;
        let offset = self.reveal_grid_focus(focus);
        iced::widget::operation::scroll_to(
            crate::ui::browser::grid::tabs_scroll_id(),
            iced::widget::operation::AbsoluteOffset {
                x: Some(offset),
                y: None,
            },
        )
    }

    pub(crate) fn browser_grid_toggle_bookmark(
        &mut self,
        id: crate::browser::engine::TabId,
    ) -> Task<Message> {
        let Some(page) = self.browser.panel.tab_page(id) else {
            return Task::none();
        };
        let url = page.session.url().to_string();
        if url.is_empty() || url == crate::browser::BLANK_PAGE_URL {
            return Task::none();
        }
        let title = page
            .session
            .title()
            .map(str::to_string)
            .unwrap_or_else(|| crate::ui::browser::display_url(&url));
        let bookmarked = self.browser.panel.toggle_bookmark(&url, &title);
        self.play_ui_cue(UiSoundKind::Action);
        crate::platform::trace::event(
            crate::platform::trace::Lane::Browser,
            "browser.bookmark.toggle",
        )
        .field("tab", id.0)
        .field("bookmarked", bookmarked)
        .emit();
        self.invalidate_browser_window()
    }

    pub(crate) fn browser_grid_open_bookmark(&mut self, index: usize) -> Task<Message> {
        use crate::ui::browser::grid::GridFocus;
        if self.browser.panel.grid.animating() || !self.browser.panel.grid.is_grid() {
            return Task::none();
        }
        let Some(bookmark) = self.browser.panel.bookmarks.get(index).cloned() else {
            return Task::none();
        };
        if self.browser.panel.order.len() >= crate::browser::tabs::MAX_SAVED_TABS {
            return Task::none();
        }
        // Bookmarks open in a new tab (active); the page expands from that
        // tab's card, which close_browser_grid_with scrolls into view.
        let open = self.open_browser_tab(Some(bookmark.url));
        let id = self.browser.panel.active_tab;
        // `open_browser_tab` already cues; the close FLIP stays silent.
        let close = match self.close_browser_grid_with(Some(id), Some(GridFocus::Tab(id))) {
            GridCloseOutcome::Started(task) => task,
            GridCloseOutcome::Rejected => Task::none(),
        };
        open.chain(close)
    }

    pub(crate) fn browser_grid_move_focus(
        &mut self,
        dir: crate::browser::cursor::JumpDir,
    ) -> Task<Message> {
        use crate::browser::cursor::JumpDir;
        use crate::ui::browser::grid::{self, GridFocus};

        if !self.browser.panel.grid.is_grid() || self.browser.panel.grid.animating() {
            return Task::none();
        }
        let panel = &self.browser.panel;
        let focus = panel.grid.focus;
        let bookmark_count = panel.bookmarks.len();
        let visual = panel.grid.visual.clone();
        let tabs = grid::grid_tabs(panel, &visual);
        let count = tabs.len() + 1;
        let slot = match focus {
            GridFocus::Plus | GridFocus::Bookmark(_) => 0,
            GridFocus::Tab(id) => tabs
                .iter()
                .position(|tab| tab.id == id)
                .map(|index| index + 1)
                .unwrap_or(0),
        };
        let mut next = focus;
        match focus {
            GridFocus::Bookmark(current) => match dir {
                JumpDir::Left if current > 0 => {
                    next = GridFocus::Bookmark(current - 1);
                }
                JumpDir::Right if current + 1 < bookmark_count => {
                    next = GridFocus::Bookmark(current + 1);
                }
                JumpDir::Up => {
                    next = match panel.grid.row_focus {
                        GridFocus::Tab(id) if visual.contains(&id) => GridFocus::Tab(id),
                        GridFocus::Plus => GridFocus::Plus,
                        _ => visual
                            .first()
                            .copied()
                            .map(GridFocus::Tab)
                            .unwrap_or(GridFocus::Plus),
                    };
                }
                _ => {}
            },
            GridFocus::Plus | GridFocus::Tab(_) => match dir {
                JumpDir::Left if slot > 0 => {
                    next = if slot == 1 {
                        GridFocus::Plus
                    } else {
                        GridFocus::Tab(tabs[slot - 2].id)
                    };
                }
                JumpDir::Right if slot + 1 < count => {
                    next = GridFocus::Tab(tabs[slot].id);
                }
                JumpDir::Down if bookmark_count > 0 => {
                    next = GridFocus::Bookmark(0);
                }
                _ => {}
            },
        }
        if next == focus {
            return Task::none();
        }
        let entering_bookmarks =
            matches!(next, GridFocus::Bookmark(_)) && !matches!(focus, GridFocus::Bookmark(_));
        self.browser.panel.grid.focus = next;
        if entering_bookmarks {
            self.browser.panel.grid.row_focus = focus;
        } else if !matches!(next, GridFocus::Bookmark(_)) {
            self.browser.panel.grid.row_focus = next;
        }
        // The pad path owns the cue (`on_browser_grid_pad`); keeping one here
        // too made every D-pad focus move play sound/haptics twice.
        let scroll = match next {
            GridFocus::Bookmark(index) => self.grid_scroll_to_bookmark(index),
            _ => self.scroll_grid_focus_into_view(),
        };
        scroll.chain(self.invalidate_browser_window())
    }

    fn grid_scroll_to_bookmark(&mut self, index: usize) -> Task<Message> {
        let view_w = self.browser_tabs_geometry().layout.bookmarks.width;
        let x = crate::ui::browser::grid::bookmark_x(index);
        let max =
            (crate::ui::browser::grid::bookmark_content_width(self.browser.panel.bookmarks.len())
                - view_w)
                .max(0.0);
        let current = self.browser.panel.grid.bookmark_scroll_x.clamp(0.0, max);
        let scroll = if x < current {
            x
        } else if x + crate::ui::browser::grid::BOOKMARK_CARD_W > current + view_w {
            x + crate::ui::browser::grid::BOOKMARK_CARD_W - view_w
        } else {
            current
        }
        .clamp(0.0, max);
        self.browser.panel.grid.bookmark_scroll_x = scroll;
        iced::widget::operation::scroll_to(
            crate::ui::browser::grid::bookmark_scroll_id(),
            iced::widget::operation::AbsoluteOffset {
                x: Some(scroll),
                y: None,
            },
        )
    }

    pub(crate) fn browser_grid_scroll_from_pad(
        &mut self,
        readings: &[crate::ui::launcher::input::NavReading],
    ) -> Task<Message> {
        if !self.browser.panel.grid.is_grid() || self.browser.panel.grid.animating() {
            return Task::none();
        }
        let Some(reading) = crate::domain::pad::preferred_reading(readings) else {
            return Task::none();
        };
        if reading.sample.rstick_x.abs() <= 0.2 {
            return Task::none();
        }
        let target = match self.browser.panel.grid.focus {
            crate::ui::browser::grid::GridFocus::Bookmark(_) => {
                crate::ui::browser::grid::bookmark_scroll_id()
            }
            _ => crate::ui::browser::grid::tabs_scroll_id(),
        };
        iced::widget::operation::scroll_by(
            target,
            iced::widget::operation::AbsoluteOffset {
                x: reading.sample.rstick_x * 42.0,
                y: 0.0,
            },
        )
    }

    /// Browser-window message router. Phase 2 is moving the reducer here; the
    /// methods still live beside the Launcher reducer until the move completes.
    pub(crate) fn on_browser_message(
        &mut self,
        message: crate::ui::browser::BrowserMessage,
    ) -> Task<Message> {
        use crate::ui::browser::BrowserMessage;
        match message {
            BrowserMessage::Back => self.on_browser_go_back(),
            BrowserMessage::Forward => self.on_browser_go_forward(),
            BrowserMessage::Reload => self.on_browser_reload(),
            BrowserMessage::PagePointerMoved { x, y } => self.on_browser_page_pointer_moved(x, y),
            BrowserMessage::PagePress => self.on_browser_page_press(),
            BrowserMessage::PageRelease => self.on_browser_page_release(),
            BrowserMessage::PagePointerLeft => self.on_browser_page_pointer_left(),
            BrowserMessage::PageWheel { dx, dy } => self.on_browser_page_wheel(dx, dy),
            BrowserMessage::Scrolled(y) => {
                self.browser.panel.note_scrolled(y);
                Task::none()
            }
            BrowserMessage::ToggleGrid => self.toggle_browser_grid(),
            BrowserMessage::GridActivate(id) => self.browser_grid_select(id),
            BrowserMessage::GridOpenPlus => self.browser_grid_open_plus(),
            BrowserMessage::GridCloseTab(id) => self.browser_grid_close_tab(id),
            BrowserMessage::GridToggleBookmark(id) => self.browser_grid_toggle_bookmark(id),
            BrowserMessage::GridOpenBookmark(index) => self.browser_grid_open_bookmark(index),
            BrowserMessage::GridAnimDone(generation) => {
                self.finish_browser_grid_transition(generation)
            }
            BrowserMessage::GridLayoutFrame => self
                .apply_pending_grid_scroll()
                .chain(self.invalidate_browser_window()),
            BrowserMessage::TabsScrolled(x) => {
                self.browser.panel.grid.tab_scroll_x = x.max(0.0);
                Task::none()
            }
            BrowserMessage::BookmarksScrolled(x) => {
                self.browser.panel.grid.bookmark_scroll_x = x.max(0.0);
                Task::none()
            }
            #[cfg(debug_assertions)]
            BrowserMessage::Shot(shot) => self.save_browser_shot(shot),
            #[cfg(debug_assertions)]
            BrowserMessage::Capture => self.capture_browser_window(),
        }
    }

    fn on_browser_page_pointer_moved(&mut self, x: f32, y: f32) -> Task<Message> {
        if self.browser.panel.url_editing {
            return self.cancel_browser_url_edit();
        }
        if !crate::platform::debug_quiet::input_dropped() {
            self.inject_browser_move(x, y);
        }
        Task::none()
    }

    fn on_browser_page_press(&mut self) -> Task<Message> {
        if self.browser.panel.url_editing {
            return self.cancel_browser_url_edit();
        }
        if crate::platform::debug_quiet::input_dropped() {
            return Task::none();
        }
        let (x, y) = self.browser.panel.page_mouse;
        self.inject_browser_press(x, y, "mouse");
        Task::none()
    }

    fn on_browser_page_release(&mut self) -> Task<Message> {
        if crate::platform::debug_quiet::input_dropped() {
            return Task::none();
        }
        let (x, y) = self.browser.panel.page_mouse;
        self.inject_browser_release(x, y, "mouse");
        Task::none()
    }

    fn on_browser_page_pointer_left(&mut self) -> Task<Message> {
        if crate::platform::debug_quiet::input_dropped() {
            return Task::none();
        }
        let (x, y) = self.browser.panel.page_mouse;
        self.inject_browser_pointer_left(x, y, "mouse");
        Task::none()
    }

    fn on_browser_page_wheel(&mut self, dx: i32, dy: i32) -> Task<Message> {
        if self.browser.panel.url_editing {
            return self.cancel_browser_url_edit();
        }
        if crate::platform::debug_quiet::input_dropped() {
            return Task::none();
        }
        let (x, y) = self.browser.panel.page_mouse;
        self.inject_browser_wheel(x, y, dx, dy, "mouse");
        Task::none()
    }

    /// Browser text-session client: insert committed text into the focused
    /// page field through CEF's key path.
    pub(crate) fn browser_text_insert(&mut self, text: &str) {
        let events = crate::browser::keys::synthetic_text(text, iced::keyboard::Modifiers::empty());
        for raw in events {
            let _ = self.on_browser_key(raw);
        }
    }

    /// Browser text-session client: backspace in the focused page field.
    pub(crate) fn browser_text_backspace(&mut self) {
        if let Some(events) =
            crate::browser::keys::synthetic_named("Backspace", iced::keyboard::Modifiers::empty())
        {
            for raw in events {
                let _ = self.on_browser_key(raw);
            }
        }
    }

    /// Browser text-session client: Enter in the focused page field.
    pub(crate) fn browser_text_enter(&mut self) {
        if let Some(events) =
            crate::browser::keys::synthetic_named("Enter", iced::keyboard::Modifiers::empty())
        {
            for raw in events {
                let _ = self.on_browser_key(raw);
            }
        }
    }

    /// Shared page-pointer move path for hardware and replay injection.
    fn inject_browser_move(&mut self, x: f32, y: f32) {
        let (x, y) = self.clamp_page_mouse(x, y);
        self.browser.panel.page_mouse_active = true;
        if let Some(engine) = &self.browser.engine {
            engine.send(EngineCommand::MouseMove {
                tab: self.browser_tab(),
                x,
                y,
                modifiers: self.browser.panel.page_mouse_modifiers,
            });
        }
    }

    /// Shared page-button-down path for hardware and replay injection.
    pub(crate) fn inject_browser_press(&mut self, x: f32, y: f32, source: &'static str) {
        let (x, y) = self.clamp_page_mouse(x, y);
        self.browser.panel.page_mouse_active = true;
        let count = self.next_page_click_count(x, y);
        self.browser.panel.page_click_count = count;
        self.browser.panel.page_mouse_down = true;
        crate::platform::trace::event(crate::platform::trace::Lane::Browser, "browser.page_press")
            .field("x", x)
            .field("y", y)
            .field("count", count)
            .field("source", source)
            .emit();
        if let Some(engine) = &self.browser.engine {
            engine.send(EngineCommand::MouseButton {
                tab: self.browser_tab(),
                x,
                y,
                button: crate::browser::engine::BrowserMouseButton::Left,
                pressed: true,
                click_count: count,
                modifiers: self.browser.panel.page_mouse_modifiers,
            });
        }
    }

    /// Shared page-button-up path. Uses the count from the matching press so
    /// Chromium sees the same click count on down and up.
    pub(crate) fn inject_browser_release(&mut self, x: f32, y: f32, source: &'static str) {
        let (x, y) = self.clamp_page_mouse(x, y);
        self.browser.panel.page_mouse_active = true;
        if !self.browser.panel.page_mouse_down {
            return;
        }
        self.browser.panel.page_mouse_down = false;
        let count = self.browser.panel.page_click_count.max(1);
        crate::platform::trace::event(
            crate::platform::trace::Lane::Browser,
            "browser.page_release",
        )
        .field("x", x)
        .field("y", y)
        .field("count", count)
        .field("source", source)
        .emit();
        if let Some(engine) = &self.browser.engine {
            engine.send(EngineCommand::MouseButton {
                tab: self.browser_tab(),
                x,
                y,
                button: crate::browser::engine::BrowserMouseButton::Left,
                pressed: false,
                click_count: count,
                modifiers: self.browser.panel.page_mouse_modifiers,
            });
        }
    }

    /// One full trusted left click at an explicit page point.
    #[cfg(debug_assertions)]
    pub(crate) fn inject_browser_click(&mut self, x: f32, y: f32, source: &'static str) {
        self.inject_browser_press(x, y, source);
        self.inject_browser_release(x, y, source);
    }

    /// Synthetic drag: down at the start, interpolated moves with the button
    /// flag, then up at the end. Exercises text selection without OS input.
    #[cfg(debug_assertions)]
    pub(crate) fn inject_browser_drag(
        &mut self,
        x1: f32,
        y1: f32,
        x2: f32,
        y2: f32,
        source: &'static str,
    ) {
        self.inject_browser_press(x1, y1, source);
        for step in 1..=6 {
            let t = step as f32 / 6.0;
            self.inject_browser_move(x1 + (x2 - x1) * t, y1 + (y2 - y1) * t);
        }
        self.inject_browser_release(x2, y2, source);
    }

    /// Pointer left the page: finish any drag and clear CEF hover.
    fn inject_browser_pointer_left(&mut self, x: f32, y: f32, source: &'static str) {
        if self.browser.panel.page_mouse_down {
            self.inject_browser_release(x, y, source);
        }
        if let Some(engine) = &self.browser.engine {
            engine.send(EngineCommand::MouseLeave {
                tab: self.browser_tab(),
            });
        }
    }

    /// Shared page-wheel path for hardware and replay injection.
    pub(crate) fn inject_browser_wheel(
        &mut self,
        x: f32,
        y: f32,
        dx: i32,
        dy: i32,
        source: &'static str,
    ) {
        let (x, y) = self.clamp_page_mouse(x, y);
        self.browser.panel.page_mouse_active = true;
        crate::platform::trace::event(crate::platform::trace::Lane::Browser, "browser.page_wheel")
            .field("x", x)
            .field("y", y)
            .field("dx", dx)
            .field("dy", dy)
            .field("source", source)
            .emit();
        let Some(engine) = &self.browser.engine else {
            // Reader fallback has no CEF wheel path; invert to iced's
            // scroll-down-positive convention.
            let _ = self.scroll_browser(-(dy as f32));
            return;
        };
        engine.send(EngineCommand::MouseWheel {
            tab: self.browser_tab(),
            x,
            y,
            dx,
            dy,
        });
    }

    /// Double/triple-click count for a press at `(x, y)`. Chromium treats the
    /// second and third presses as word/paragraph selection.
    fn next_page_click_count(&mut self, x: f32, y: f32) -> u8 {
        let (next, stamp) =
            next_click_count(self.browser.panel.page_last_click, Instant::now(), x, y);
        self.browser.panel.page_last_click = stamp;
        next
    }

    fn clamp_page_mouse(&mut self, x: f32, y: f32) -> (f32, f32) {
        let (w, h) = self.browser.panel.viewport;
        let x = x.clamp(0.0, (w - 1.0).max(0.0));
        let y = y.clamp(0.0, (h - 1.0).max(0.0));
        self.browser.panel.page_mouse = (x, y);
        (x, y)
    }

    /// Window focus loss: finish a drag and drop keyboard modifiers so a
    /// later page move cannot inherit a stale Shift/Ctrl state.
    pub(crate) fn browser_pointer_focus_lost(&mut self) {
        self.browser.panel.page_mouse_modifiers = keyboard_types::Modifiers::empty();
        let (x, y) = self.browser.panel.page_mouse;
        self.inject_browser_pointer_left(x, y, "unfocus");
    }

    /// Replay text injection: press+release each character through the real
    /// browser key handler. Never touches the OS keyboard. Tasks are chained so
    /// header/OSK invalidations and commits land before the next replay step.
    #[cfg(debug_assertions)]
    pub(crate) fn replay_type_text(&mut self, text: &str) -> Task<Message> {
        let events = crate::browser::keys::synthetic_text(text, iced::keyboard::Modifiers::empty());
        let task = self.replay_raw_keys(events, "text");
        crate::platform::trace::event(
            crate::platform::trace::Lane::Browser,
            "browser.synthetic_text",
        )
        .field("len", text.chars().count())
        .emit();
        task
    }

    /// Replay named-key tap through the real browser key handler. `None` means
    /// the name is not a known named key (the caller may treat it as text).
    #[cfg(debug_assertions)]
    pub(crate) fn replay_press_key(
        &mut self,
        name: &str,
        modifiers: iced::keyboard::Modifiers,
    ) -> Option<Task<Message>> {
        let events = crate::browser::keys::synthetic_named(name, modifiers)?;
        Some(self.replay_raw_keys(events.to_vec(), name))
    }

    #[cfg(debug_assertions)]
    fn replay_raw_keys(
        &mut self,
        events: Vec<crate::browser::keys::RawKey>,
        label: &str,
    ) -> Task<Message> {
        let mut task = Task::none();
        for raw in events {
            task = task.chain(self.on_browser_key(raw));
        }
        crate::platform::trace::event(
            crate::platform::trace::Lane::Browser,
            "browser.synthetic_key",
        )
        .field("key", label)
        .emit();
        task
    }
}

impl super::App {
    /// Show the standalone Browser window (idempotent). Restores the persisted
    /// saved page on a fresh session, else resumes the cached page.
    pub(crate) fn open_browser_window(&mut self) -> Task<Message> {
        self.clear_ps_resume();
        if self.browser_visible
            && let Some(id) = self.shell_window
        {
            return crate::ui::layout::focus_window(id);
        }
        if self.browser.engine.is_none() {
            crate::controller::hid::diag::diag_info(
                "ui-diag: browser open skipped (engine not installed)",
            );
            return Task::none();
        }
        // The outgoing screen keeps the native cursor hidden across the swap.
        self.shell_opening = true;
        self.reset_shell_cursor_clock();
        let cover =
            crate::ui::layout::primary_monitor_cover().unwrap_or(crate::ui::layout::MonitorCover {
                x: 0.0,
                y: 0.0,
                width: 1920.0,
                height: 1080.0,
            });
        self.browser_monitor_cover = Some(cover);
        // The cover drives the first layout until a resize event arrives.
        self.browser_window_size = None;
        self.browser.panel.l1_held = false;
        self.browser.panel.r1_held = false;
        self.browser.panel.triangle_held = false;
        self.sync_browser_viewport();
        // Center before the first frame, not after the window has painted the
        // previous open's cursor position.
        self.browser.panel.cursor.reset_for_page();
        let start_in_grid = std::mem::take(&mut self.browser.start_in_grid);
        let was_open = self.browser.panel.session.is_open();
        self.consume_ps_release();
        crate::controller::hid::diag::diag_info("ui-diag: browser window open");
        let activation = self.activate_shell_mode(crate::app::home::ShellScreen::Browser, cover);
        let session = if was_open {
            // Reopen after close: cached page, suspended engine.
            self.browser.panel.session.open_surface();
            self.resume_browser_engine();
            activation
        } else {
            // Fresh surface: restore the saved page or open the default page.
            activation.chain(self.open_browser_session())
        };
        if start_in_grid {
            let prepare = self.prepare_browser_grid_on_open();
            return session.chain(prepare);
        }
        session
    }

    /// Home tile opens the Browser directly on its tabs view. The page is
    /// still live behind it; selecting a card runs the normal FLIP into page
    /// mode.
    fn prepare_browser_grid_on_open(&mut self) -> Task<Message> {
        use crate::ui::browser::grid::GridFocus;
        let panel = &mut self.browser.panel;
        panel.cancel_url_edit();
        panel.grid.mode = crate::ui::browser::grid::GridMode::Grid;
        panel.grid.anim = None;
        panel.grid.bookmark_scroll_x = 0.0;
        panel.refresh_grid_visual();
        self.browser.grid_nav.prepare_on_open(true);
        self.browser.pending_grid_move = None;
        self.browser.dpad_stepper.reset();
        let active = self.browser.panel.active_tab;
        let offset = self.reveal_grid_focus(GridFocus::Tab(active));
        self.browser.grid_scroll_pending = Some(offset);
        self.pending_grid_scroll_later()
    }

    /// The strip scrollable only exists after the first frame; apply the
    /// pending offset on the next tick instead of dropping the operation.
    fn pending_grid_scroll_later(&self) -> Task<Message> {
        if self.browser.grid_scroll_pending.is_none() {
            return Task::none();
        }
        Task::perform(super::delay(std::time::Duration::from_millis(16)), |_| {
            Message::Browser(crate::ui::browser::BrowserMessage::GridLayoutFrame)
        })
    }

    /// Apply the strip offset saved when the grid opened, now that the
    /// scrollable is in the widget tree.
    fn apply_pending_grid_scroll(&mut self) -> Task<Message> {
        let Some(offset) = self.browser.grid_scroll_pending.take() else {
            return Task::none();
        };
        self.browser.panel.grid.tab_scroll_x = offset;
        iced::widget::operation::scroll_to(
            crate::ui::browser::grid::tabs_scroll_id(),
            iced::widget::operation::AbsoluteOffset {
                x: Some(offset),
                y: None,
            },
        )
    }

    /// True while the Browser card is animating out.
    pub(crate) fn browser_closing(&self) -> bool {
        self.browser_shell_anim
            .is_some_and(|anim| anim.dir == crate::ui::home::ShellDir::Exit)
    }

    /// Close the Browser screen. The shared shell window stays alive: a swap
    /// switches mode after the card fades; PS dismiss hides the warm window.
    pub(crate) fn close_browser_window(&mut self) -> Task<Message> {
        if self.browser_closing() {
            return Task::none();
        }
        if self.browser_window.is_none() {
            return Task::none();
        }
        self.browser_visible = false;
        // Hiding the warm shell restores the pointer during the exit fade.
        let last_shell = !self.home_visible && !self.launcher_visible && !self.shell_opening;
        if self.shell_cursor_hidden && last_shell {
            self.set_shell_cursor_hidden(false);
        }
        self.browser_monitor_cover = None;
        self.browser_window_size = None;
        self.reset_shell_cursor_clock();
        let now = Instant::now();
        if !self.shell_opening && self.pending_shell_mode.is_none() {
            self.begin_shell_backdrop_exit(now);
        } else {
            self.shell_backdrop.hold_full();
        }
        let close_osk = self.close_osk("browser_close");
        let Some(id) = self.shell_window else {
            self.browser_shell_anim = None;
            return close_osk;
        };
        let reduced =
            crate::ui::motion::reduced_motion_effective(self.session.prefs.reduced_motion);
        let from = self
            .browser_shell_anim
            .map(|anim| anim.frame(now, reduced))
            .unwrap_or_else(crate::ui::home::ShellFrame::full);
        self.browser_shell_anim = Some(crate::ui::home::ShellAnim::exit(now, from));
        close_osk.chain(crate::ui::layout::invalidate_window(id))
    }

    /// Second Browser close phase: the card finished fading. Switch to the
    /// screen that was waiting, or hide the warm shell for the desktop.
    pub(crate) fn finish_browser_window_close(&mut self) -> Task<Message> {
        self.browser_shell_anim = None;
        self.browser_window = None;
        crate::controller::hid::diag::diag_info("ui-diag: browser window close");
        let surface = self.close_browser_surface();
        if self.ps_resume_pending {
            self.pending_shell_mode = None;
            return surface.chain(self.hide_shell_window());
        }
        if let Some(mode) = self.take_pending_shell_mode() {
            return surface.chain(self.switch_shell_mode(mode));
        }
        if self.home_visible || self.launcher_visible {
            return surface;
        }
        surface.chain(self.hide_shell_window())
    }

    pub(crate) fn on_browser_window_sized(&mut self, size: iced::Size) -> Task<Message> {
        let size = (size.width.max(1.0), size.height.max(1.0));
        // The Browser window is a borderless fullscreen window sized to the
        // monitor cover. iced configures the surface once at its pre-fullscreen
        // default (observed 1440x789) before the final resize; adopting that
        // would briefly shrink the card, engine viewport, and cursor space.
        // The cover is the intended size, so ignore any other size.
        if let Some(cover) = self.browser_monitor_cover
            && ((size.0 - cover.width).abs() > 1.0 || (size.1 - cover.height).abs() > 1.0)
        {
            return Task::none();
        }
        if self.browser_window_size == Some(size) {
            return Task::none();
        }
        self.browser_window_size = Some(size);
        crate::controller::hid::diag::diag_info(format!(
            "ui-diag: browser window size {:.0}x{:.0}",
            size.0, size.1
        ));
        self.sync_browser_viewport();
        Task::none()
    }
}

impl super::App {
    /// Pad tick while the standalone Browser window is the owner. Home is
    /// routed before this, so the browser never sees Home presses.
    pub(crate) fn on_browser_pad(
        &mut self,
        readings: &[crate::ui::launcher::input::NavReading],
        tap_actions: &[PadAction],
    ) -> Task<Message> {
        use crate::browser::cursor::JumpDir;
        self.browser.panel.l1_held = readings.iter().any(|reading| reading.sample.l1);
        self.browser.panel.r1_held = readings.iter().any(|reading| reading.sample.r1);
        self.browser.panel.triangle_held = readings.iter().any(|reading| reading.sample.triangle);
        if readings.is_empty() {
            return Task::none();
        }
        if readings
            .iter()
            .any(|reading| !crate::domain::pad::sample_nav_resting(&reading.sample))
        {
            // Real pad input takes pointer ownership back from the mouse.
            self.browser.panel.page_mouse_active = false;
        }
        // PS is global: while Browser owns the pad it dismisses the screen
        // (the next PS restores this same Browser).
        if self.ps_released(readings) {
            crate::controller::hid::diag::diag_info("ui-diag: PS dismisses Browser");
            return self.ps_screen_toggle();
        }
        if self.browser.panel.grid.is_grid() {
            return self.on_browser_grid_pad(readings, tap_actions);
        }
        if tap_actions.iter().any(|action| {
            action.button == PadButton::Create && action.action == PadButtonAction::Tap
        }) {
            crate::controller::hid::diag::diag_info("ui-diag: Share opens browser grid");
            return self.open_browser_grid();
        }
        let now = std::time::Instant::now();
        let engine_mode = self.browser.panel.session.has_frame();
        // Reader fallback uses the D-pad nav bank; engine mode parks it
        // (cursor jumps run on the dedicated stepper).
        let allow_nav = !engine_mode;
        let mut tick = self.browser.nav.tick(
            readings, now, allow_nav, true, false, false, false, false, false,
        );
        // Source-side taps replace the old per-surface face-edge reconstruction.
        if let Some((pad, action)) = browser_tap_action(tap_actions) {
            tick.action = Some(action);
            tick.action_pad = Some(pad);
        }
        let semantic = self.hosted_action_for_tap(tap_actions);
        self.set_haptic_pad_from_id(
            tick.action_pad
                .as_ref()
                .or(tick.cross_completed_pad.as_ref())
                .or(tick.triangle_completed_pad.as_ref()),
        );
        if engine_mode {
            if let Some((jump, pad)) = self.poll_browser_dpad_jump(readings, now) {
                self.set_haptic_pad_from_id(Some(&pad));
                self.play_ui_cue(UiSoundKind::Nav);
                self.browser.panel.jump_cursor(jump);
            }
            // R3 issue flag (R3 carries no NavAction).
            let r3_down = readings.iter().any(|r| {
                r.sample
                    .held
                    .contains(&crate::ui::launcher::gesture::GestureControl::R3)
            });
            let latched = std::mem::replace(&mut self.browser.panel.r3_latched, r3_down);
            let flag = if r3_down && !latched {
                self.flag_browser_issue(now)
            } else {
                Task::none()
            };
            let action = if let Some(invoked) = semantic {
                self.invoke_hosted_action(invoked)
            } else {
                match tick.action {
                    Some(NavAction::Up) => {
                        self.play_ui_cue(UiSoundKind::Nav);
                        self.browser.panel.jump_cursor(JumpDir::Up);
                        self.invalidate_browser_window()
                    }
                    Some(NavAction::Down) => {
                        self.play_ui_cue(UiSoundKind::Nav);
                        self.browser.panel.jump_cursor(JumpDir::Down);
                        self.invalidate_browser_window()
                    }
                    Some(NavAction::Left) => {
                        self.play_ui_cue(UiSoundKind::Nav);
                        self.browser.panel.jump_cursor(JumpDir::Left);
                        self.invalidate_browser_window()
                    }
                    Some(NavAction::Right) => {
                        self.play_ui_cue(UiSoundKind::Nav);
                        self.browser.panel.jump_cursor(JumpDir::Right);
                        self.invalidate_browser_window()
                    }
                    Some(NavAction::Confirm) => self.click_browser_cursor(),
                    // Triangle is the URL editor; page navigation itself
                    // never uses it.
                    Some(NavAction::Triangle) => self.on_browser_url_edit(),
                    // Circle closes the Browser unless the URL well owns the
                    // pad; in that case it cancels the edit first.
                    Some(NavAction::Cancel) => self.on_browser_circle(),
                    Some(NavAction::ToggleMagnetism) => {
                        self.play_ui_cue(UiSoundKind::Action);
                        let magnet = !self.browser.panel.cursor.snapshot().magnet;
                        self.browser.panel.cursor.set_magnet(magnet);
                        crate::platform::app_log::info(format!(
                            "ui-diag: browser magnet {}",
                            if magnet { "on" } else { "off" }
                        ));
                        self.invalidate_browser_window()
                    }
                    _ => Task::none(),
                }
            };
            Task::batch(vec![flag, action])
        } else {
            // Reader fallback: D-pad scrolls text, Cross restores the page.
            if let Some(invoked) = semantic {
                return self.invoke_hosted_action(invoked);
            }
            match tick.action {
                Some(NavAction::Up) | Some(NavAction::Left) => {
                    self.play_ui_cue(UiSoundKind::Nav);
                    self.scroll_browser(-crate::ui::browser::view::BROWSER_SCROLL_STEP)
                }
                Some(NavAction::Down) | Some(NavAction::Right) => {
                    self.play_ui_cue(UiSoundKind::Nav);
                    self.scroll_browser(crate::ui::browser::view::BROWSER_SCROLL_STEP)
                }
                Some(NavAction::Confirm) => self.on_browser_go_default(),
                Some(NavAction::Triangle) => self.on_browser_url_edit(),
                Some(NavAction::Cancel) => self.on_browser_circle(),
                _ => Task::none(),
            }
        }
    }

    /// Grid-mode pad routing. D-pad moves card focus, right stick scrolls the
    /// grid / bookmark strip, and face taps map to card actions. Page input is
    /// fully parked while the grid owns the surface.
    pub(crate) fn on_browser_grid_pad(
        &mut self,
        readings: &[crate::ui::launcher::input::NavReading],
        tap_actions: &[PadAction],
    ) -> Task<Message> {
        use crate::browser::cursor::JumpDir;
        if self.browser.panel.grid.animating() {
            if self.browser.panel.grid.mode == crate::ui::browser::grid::GridMode::Entering
                && let Some(mv) = grid_direction(readings)
            {
                self.browser.pending_grid_move = Some(mv);
            }
            return Task::none();
        }
        let previous_haptic = self.haptic_pad_serial.take();
        let mut task = Task::none();
        if !readings.is_empty() {
            // Run the shared per-pad bank, not a bare stepper: it supplies the
            // MENU arming/neutral grace and per-pad repeat.
            let grid_readings = grid_nav_readings(readings);
            let tick = self.browser.grid_nav.tick(
                &grid_readings,
                std::time::Instant::now(),
                true,
                true,
                false,
                false,
                false,
                false,
                false,
            );
            if let Some(pad) = tick.action_pad.as_ref() {
                self.set_haptic_pad_from_id(Some(pad));
            }
            let jump = match tick.action {
                Some(NavAction::Up) => Some(JumpDir::Up),
                Some(NavAction::Down) => Some(JumpDir::Down),
                Some(NavAction::Left) => Some(JumpDir::Left),
                Some(NavAction::Right) => Some(JumpDir::Right),
                _ => None,
            };
            if let Some(jump) = jump {
                self.play_ui_cue(UiSoundKind::Nav);
                task = task.chain(self.browser_grid_move_focus(jump));
            }
        }
        let button = tap_actions
            .iter()
            .find(|action| action.action == PadButtonAction::Tap);
        if let Some(action) = button {
            self.set_haptic_pad_from_id(Some(&action.pad));
            let action = match action.button {
                // Share/Create is page-only; in tabs view it is unbound.
                PadButton::Create => Task::none(),
                PadButton::Circle => self.return_to_home_screen(),
                PadButton::Cross => self.browser_grid_activate_focus(),
                PadButton::Square => self.browser_grid_close_focused_tab(),
                PadButton::Triangle => self.browser_grid_toggle_focused_bookmark(),
                _ => Task::none(),
            };
            task = task.chain(action);
        }
        task = task.chain(self.browser_grid_scroll_from_pad(readings));
        self.haptic_pad_serial = previous_haptic;
        task
    }

    fn browser_grid_activate_focus(&mut self) -> Task<Message> {
        use crate::ui::browser::grid::GridFocus;
        match self.browser.panel.grid.focus {
            GridFocus::Tab(id) => self.close_browser_grid(Some(id)),
            GridFocus::Plus => self.browser_grid_open_plus(),
            GridFocus::Bookmark(index) => self.browser_grid_open_bookmark(index),
        }
    }

    fn browser_grid_close_focused_tab(&mut self) -> Task<Message> {
        use crate::ui::browser::grid::GridFocus;
        match self.browser.panel.grid.focus {
            GridFocus::Tab(id) => self.browser_grid_close_tab(id),
            _ => Task::none(),
        }
    }

    fn browser_grid_toggle_focused_bookmark(&mut self) -> Task<Message> {
        use crate::ui::browser::grid::GridFocus;
        match self.browser.panel.grid.focus {
            GridFocus::Tab(id) => self.browser_grid_toggle_bookmark(id),
            GridFocus::Bookmark(index) => {
                if self.browser.panel.remove_bookmark(index) {
                    self.play_ui_cue(UiSoundKind::Action);
                    let len = self.browser.panel.bookmarks.len();
                    self.browser.panel.grid.focus = if len == 0 {
                        GridFocus::Tab(self.browser.panel.active_tab)
                    } else {
                        GridFocus::Bookmark(index.min(len - 1))
                    };
                    return self.invalidate_browser_window();
                }
                Task::none()
            }
            GridFocus::Plus => Task::none(),
        }
    }

    /// Escape cancels URL editing but never closes the Browser.
    pub(crate) fn browser_cancel(&mut self) -> Task<Message> {
        if self.browser.panel.url_editing {
            return self.cancel_browser_url_edit();
        }
        Task::none()
    }

    /// Circle: cancel URL editing when it owns the pad, else suspend the
    /// Browser and return to the Home screen.
    pub(crate) fn on_browser_circle(&mut self) -> Task<Message> {
        if self.browser.panel.url_editing {
            return self.cancel_browser_url_edit();
        }
        // Page -> tabs. Tabs -> Home is handled by the grid pad path.
        crate::platform::trace::event(crate::platform::trace::Lane::Browser, "browser.grid.open")
            .field("source", "circle")
            .emit();
        self.open_browser_grid()
    }

    /// Open the URL editor (Triangle). If already editing, commit instead.
    pub(crate) fn on_browser_url_edit(&mut self) -> Task<Message> {
        if self.browser.panel.url_editing {
            return self.commit_browser_url_edit();
        }
        self.browser.panel.start_url_edit();
        self.request_osk_open(crate::ui::osk::OskTarget::BrowserUrl)
    }

    /// Commit the URL draft through the same navigate funnel as page links.
    pub(crate) fn commit_browser_url_edit(&mut self) -> Task<Message> {
        if !self.browser.panel.url_editing {
            return Task::none();
        }
        let draft = self.browser.panel.url_draft.trim().to_string();
        self.browser.panel.cancel_url_edit();
        let close = if self.osk.state.open
            && self.osk.state.target == Some(crate::ui::osk::OskTarget::BrowserUrl)
        {
            self.close_osk("url_commit")
        } else {
            Task::none()
        };
        let redraw = self.invalidate_browser_window();
        if draft.is_empty() {
            return close.chain(redraw);
        }
        let navigate = self.navigate_browser_to(draft);
        // Closing the URL keyboard can leave CEF's OSR paint paused; reassert
        // engine visibility and OS focus so the committed page presents.
        self.resume_browser_engine();
        let focus = self
            .browser_window
            .map(crate::ui::layout::focus_window)
            .unwrap_or_else(Task::none);
        close.chain(navigate).chain(focus).chain(redraw)
    }

    /// Cancel URL editing and dismiss the OSK when it belongs to the URL well.
    pub(crate) fn cancel_browser_url_edit(&mut self) -> Task<Message> {
        if self.osk.state.open
            && self.osk.state.target == Some(crate::ui::osk::OskTarget::BrowserUrl)
        {
            return self.close_osk("url_cancel");
        }
        self.browser.panel.cancel_url_edit();
        self.invalidate_browser_window()
    }

    /// Force the Browser window to redraw after header-only state changes.
    /// The debug replay `text` path drops returned tasks, but the OSK/hardware
    /// editing paths chain this so the URL draft is visible as it grows.
    pub(crate) fn invalidate_browser_window(&self) -> Task<Message> {
        self.browser_window
            .map(crate::ui::layout::invalidate_window)
            .unwrap_or_else(Task::none)
    }

    /// Hardware-keyboard editing of the URL draft. The system keyboard feeds
    /// the same state through `apply_osk_*`; neither path reaches CEF while
    /// the URL well owns text.
    fn on_browser_url_key(&mut self, raw: crate::browser::keys::RawKey) -> Task<Message> {
        use iced::keyboard::Key;
        use iced::keyboard::key::Named;
        if !raw.pressed || raw.modifiers.control() || raw.modifiers.alt() {
            return Task::none();
        }
        match &raw.key {
            Key::Character(value) => {
                self.browser.panel.url_edit_insert(value);
                self.invalidate_browser_window()
            }
            Key::Named(Named::Backspace) => {
                self.browser.panel.url_edit_backspace();
                self.invalidate_browser_window()
            }
            Key::Named(Named::Enter) => self.commit_browser_url_edit(),
            Key::Named(Named::Escape) => self.cancel_browser_url_edit(),
            _ => Task::none(),
        }
    }

    /// Platform IME events for the page. These become CEF
    /// composition/commit calls.
    pub(crate) fn on_browser_ime(
        &mut self,
        id: iced::window::Id,
        event: iced::advanced::input_method::Event,
    ) -> Task<Message> {
        use iced::advanced::input_method::Event;
        if Some(id) != self.browser_window || !self.browser_visible {
            return Task::none();
        }
        if self.browser.panel.url_editing {
            return Task::none();
        }
        let tab = self.browser_tab();
        let Some(engine) = &self.browser.engine else {
            return Task::none();
        };
        match event {
            Event::Opened => {}
            Event::Closed => {
                engine.send(EngineCommand::ImeCancelComposition { tab });
            }
            Event::Preedit(text, _) => {
                crate::platform::trace::event(crate::platform::trace::Lane::Browser, "browser.ime")
                    .field("kind", "preedit")
                    .field("len", text.chars().count())
                    .emit();
                if text.is_empty() {
                    engine.send(EngineCommand::ImeCancelComposition { tab });
                } else {
                    engine.send(EngineCommand::ImeSetComposition { tab, text });
                }
            }
            Event::Commit(text) => {
                crate::platform::trace::event(crate::platform::trace::Lane::Browser, "browser.ime")
                    .field("kind", "commit")
                    .field("len", text.chars().count())
                    .emit();
                if text.is_empty() {
                    engine.send(EngineCommand::ImeFinishComposition { tab });
                } else {
                    // CEF's windowless commit path is unreliable on Windows;
                    // stage the committed text as a composition and finish it,
                    // which inserts the same text through the stable path.
                    engine.send(EngineCommand::ImeSetComposition {
                        tab,
                        text: text.clone(),
                    });
                    engine.send(EngineCommand::ImeFinishComposition { tab });
                }
            }
        }
        Task::none()
    }

    /// Keyboard while the standalone Browser window has focus.
    pub(crate) fn on_browser_key(&mut self, raw: crate::browser::keys::RawKey) -> Task<Message> {
        use crate::ui::browser::BrowserMessage;
        use iced::keyboard::Key;
        use iced::keyboard::key::Named;
        self.browser.panel.page_mouse_modifiers =
            crate::browser::keys::engine_modifiers(&raw.modifiers);
        if self.browser.panel.url_editing {
            return self.on_browser_url_key(raw);
        }
        if self.browser.panel.grid.is_grid() {
            if raw.pressed && matches!(raw.key, Key::Named(Named::Escape)) {
                return self.close_browser_grid(None);
            }
            return Task::none();
        }
        if matches!(raw.key, Key::Named(Named::Escape)) {
            if !raw.pressed {
                return Task::none();
            }
            return self.browser_cancel();
        }
        if raw.pressed {
            if raw.modifiers.control()
                && let Key::Character(text) = &raw.key
                && text.to_lowercase().as_str() == "r"
            {
                return self.on_browser_message(BrowserMessage::Reload);
            }
            if raw.modifiers.alt() {
                match &raw.key {
                    Key::Named(Named::ArrowLeft) => {
                        return self.on_browser_message(BrowserMessage::Back);
                    }
                    Key::Named(Named::ArrowRight) => {
                        return self.on_browser_message(BrowserMessage::Forward);
                    }
                    _ => {}
                }
            }
        }
        if self.browser.engine.is_some() {
            if let Some(engine) = &self.browser.engine {
                let tab = self.browser_tab();
                engine.send(crate::browser::engine::EngineCommand::Key(
                    crate::browser::engine::TabKey {
                        tab,
                        event: crate::browser::keys::map_event(&raw),
                    },
                ));
            }
            return Task::none();
        }
        // Reader fallback keyboard.
        if !raw.pressed {
            return Task::none();
        }
        match raw.key {
            Key::Named(Named::ArrowUp) => {
                self.scroll_browser(-crate::ui::browser::view::BROWSER_SCROLL_STEP)
            }
            Key::Named(Named::ArrowDown) => {
                self.scroll_browser(crate::ui::browser::view::BROWSER_SCROLL_STEP)
            }
            Key::Named(Named::Enter) => self.on_browser_go_default(),
            _ => Task::none(),
        }
    }
}

impl super::App {
    /// Re-check the optional engine package and spawn CEF if it appeared
    /// (Store install completed while the app stayed open, dev download, or a
    /// portable sidecar placed next to the exe).
    #[cfg(feature = "cef-engine")]
    pub(crate) fn refresh_engine_status(&mut self) {
        if self.browser.engine.is_some() {
            self.browser.engine_available = true;
            // The engine may already be attached at boot (sidecar/package);
            // managed state still has to be derived from the loaded path.
            if let Some(dir) = crate::browser::cef_backend::loaded_engine_dir() {
                self.browser.engine_managed = crate::browser::cef_backend::engine_is_managed(&dir);
            }
            return;
        }
        let Some(path) = crate::browser::cef_backend::resolve_engine_dir() else {
            self.browser.engine_status = EngineStatus::Missing;
            self.browser.engine_managed = false;
            return;
        };
        self.browser.engine_managed = crate::browser::cef_backend::engine_is_managed(&path);
        let (w, h) = self
            .browser_monitor_cover
            .map(|c| (c.width, c.height))
            .unwrap_or((1920.0, 1080.0));
        let (page_w, page_h) = crate::ui::browser::view::fullscreen_layout(w, h);
        if let Some(engine) =
            crate::browser::cef_backend::spawn_engine(&crate::browser::tabs::boot_url(), {
                (page_w.max(1.0) as u32, page_h.max(1.0) as u32)
            })
        {
            self.browser.engine = Some(engine);
            self.browser.engine_available = true;
            self.browser.engine_status = EngineStatus::Ready(path);
            crate::platform::app_log::info("cef: engine attached after install");
        } else {
            self.browser.engine_status =
                EngineStatus::Failed("Browser engine could not be initialized".to_string());
        }
    }

    #[cfg(not(feature = "cef-engine"))]
    pub(crate) fn refresh_engine_status(&mut self) {
        self.browser.engine_status = EngineStatus::Missing;
        self.browser.engine_available = false;
        self.browser.engine_managed = false;
    }

    /// Start the optional engine install/update. Packaged builds hand off to
    /// the Store; unpackaged/dev builds use the configured zip feed.
    #[cfg(feature = "cef-engine")]
    pub(crate) fn start_engine_install(&mut self) -> Task<Message> {
        if matches!(
            self.browser.engine_status,
            EngineStatus::Installing | EngineStatus::Uninstalling
        ) {
            return Task::none();
        }
        if crate::platform::packaged::is_packaged() {
            let url = std::env::var("SDSC_BROWSER_ENGINE_STORE_URL").unwrap_or_else(|_| {
                "ms-windows-store://search/?query=SDSC%20Utils%20Browser%20Engine".to_string()
            });
            let launched = crate::games::launch::launch_target(&url, "").map_err(|e| e.to_string());
            if let Err(err) = launched {
                crate::platform::app_log::warn(format!(
                    "engine install: could not open Store page: {err}"
                ));
            }
            let msg = "Finish installing Browser Engine in the Store, then reopen Home";
            self.browser.engine_status = EngineStatus::Missing;
            self.home_state.install_done(false, Some(msg.to_string()));
            return Task::none();
        }

        self.browser.engine_install_previous = Some(self.browser.engine_status.clone());
        self.browser.engine_status = EngineStatus::Installing;
        self.browser.engine_rate = None;
        self.home_state.set_installing(true);
        self.browser.engine_install_generation =
            self.browser.engine_install_generation.wrapping_add(1);
        let generation = self.browser.engine_install_generation;
        let cancel = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        self.browser.engine_install_cancel = std::sync::Arc::clone(&cancel);
        let limit = self.install_policy.handle();
        let (progress_tx, progress_rx) = iced::futures::channel::mpsc::unbounded::<
            crate::browser::engine_install::InstallProgress,
        >();
        let progress_task =
            Task::stream(progress_rx).map(move |progress| Message::EngineInstallProgress {
                generation,
                progress,
            });
        let install = Task::perform(
            async move {
                super::spawn_blocking(move || {
                    let mut progress = |progress| {
                        let _ = progress_tx.unbounded_send(progress);
                    };
                    match crate::browser::engine_install::install_with_progress(
                        &mut progress,
                        crate::browser::engine_install::InstallOptions {
                            download_limit_kbps: limit,
                            cancel,
                        },
                    ) {
                        Ok(path) => EngineInstallOutcome::Installed(path),
                        Err(crate::browser::engine_install::InstallError::Cancelled) => {
                            EngineInstallOutcome::Cancelled
                        }
                        Err(crate::browser::engine_install::InstallError::Failed(err)) => {
                            EngineInstallOutcome::Failed(err)
                        }
                    }
                })
                .await
            },
            move |result| Message::EngineInstallDone {
                generation,
                outcome: match result {
                    Ok(outcome) => outcome,
                    Err(err) => EngineInstallOutcome::Failed(err.to_string()),
                },
            },
        );
        Task::batch([progress_task, install])
    }

    #[cfg(not(feature = "cef-engine"))]
    pub(crate) fn start_engine_install(&mut self) -> Task<Message> {
        self.home_state.install_done(
            false,
            Some("This build has no browser engine support".to_string()),
        );
        Task::none()
    }

    #[cfg(feature = "cef-engine")]
    pub(crate) fn start_engine_uninstall(&mut self) -> Task<Message> {
        let EngineStatus::Ready(path) = &self.browser.engine_status else {
            return Task::none();
        };
        let path = path.clone();
        if crate::platform::packaged::is_packaged() {
            let _ = crate::games::launch::launch_target("ms-settings:appsfeatures", "");
            self.home_state.install_done(
                false,
                Some("Remove Browser Engine in Windows Settings, then reopen Home".to_string()),
            );
            return Task::none();
        }
        let close_browser = if self.browser_visible {
            self.close_browser_window()
        } else {
            Task::none()
        };
        self.browser.engine_status = EngineStatus::Uninstalling;
        self.home_state.set_uninstalling();
        self.browser.engine_install_generation =
            self.browser.engine_install_generation.wrapping_add(1);
        let generation = self.browser.engine_install_generation;
        let (done_tx, done_rx) = std::sync::mpsc::channel();
        if let Some(engine) = self.browser.engine.take() {
            engine.send(EngineCommand::Shutdown { done: done_tx });
        }
        self.browser.engine_available = false;
        let uninstall = Task::perform(
            async move {
                super::spawn_blocking(move || {
                    // Let the engine worker close its browsers before the
                    // pointer is dropped. The DLL stays mapped either way;
                    // the directory itself is deferred to next boot.
                    let _ = done_rx.recv_timeout(std::time::Duration::from_secs(2));
                    // Keep the inline meter visible long enough to read.
                    std::thread::sleep(std::time::Duration::from_millis(600));
                    crate::browser::engine_install::uninstall_local(&path)
                        .map_err(crate::AppError::Persist)
                })
                .await
            },
            move |result| Message::EngineUninstallDone {
                generation,
                result: match result {
                    Ok(Ok(())) => Ok(()),
                    Ok(Err(err)) => Err(err.to_string()),
                    Err(err) => Err(err.to_string()),
                },
            },
        );
        close_browser.chain(uninstall)
    }

    #[cfg(not(feature = "cef-engine"))]
    pub(crate) fn start_engine_uninstall(&mut self) -> Task<Message> {
        self.home_state.install_done(
            false,
            Some("This build has no browser engine support".to_string()),
        );
        Task::none()
    }

    /// Request a cooperative cancel of the live install/update. The worker
    /// stops at the next chunk boundary, deletes staging, and reports
    /// `Cancelled`; the generation guard then restores the prior state.
    #[cfg(feature = "cef-engine")]
    pub(crate) fn cancel_engine_install(&mut self) -> Task<Message> {
        if !matches!(self.browser.engine_status, EngineStatus::Installing)
            || self.home_state.cancelling
        {
            return Task::none();
        }
        self.browser
            .engine_install_cancel
            .store(true, std::sync::atomic::Ordering::Relaxed);
        self.home_state.set_cancelling();
        crate::platform::app_log::info("cef: BrowserEngine install cancel requested");
        Task::none()
    }

    #[cfg(not(feature = "cef-engine"))]
    pub(crate) fn cancel_engine_install(&mut self) -> Task<Message> {
        Task::none()
    }

    /// Fetch the DLC manifest in the background when the feed is configured.
    /// Called on Home open; no polling, no telemetry.
    #[cfg(feature = "cef-engine")]
    pub(crate) fn check_engine_manifest(&mut self) -> Task<Message> {
        if std::env::var_os("SDSC_CEF_DLC_MANIFEST_URL").is_none() {
            return Task::none();
        }
        self.browser.engine_manifest_generation =
            self.browser.engine_manifest_generation.wrapping_add(1);
        let generation = self.browser.engine_manifest_generation;
        Task::perform(
            async move { super::spawn_blocking(crate::browser::engine_install::fetch_manifest).await },
            move |result| Message::EngineManifestReady {
                generation,
                result: match result {
                    Ok(inner) => inner,
                    Err(err) => Err(err.to_string()),
                },
            },
        )
    }

    #[cfg(not(feature = "cef-engine"))]
    pub(crate) fn check_engine_manifest(&mut self) -> Task<Message> {
        Task::none()
    }

    #[cfg(feature = "cef-engine")]
    pub(crate) fn on_engine_manifest_ready(
        &mut self,
        generation: u64,
        result: Result<Option<crate::browser::engine_install::EngineManifest>, String>,
    ) -> Task<Message> {
        if generation != self.browser.engine_manifest_generation {
            return Task::none();
        }
        match result {
            Ok(Some(manifest)) => {
                self.browser.engine_manifest = Some(manifest);
                self.sync_home_engine_info();
            }
            Ok(None) => {}
            Err(err) => {
                crate::platform::app_log::warn(format!("cef: engine manifest check failed: {err}"));
            }
        }
        Task::none()
    }

    /// Measure the active engine and pending-cleanup directories off the UI
    /// thread for the Info panel.
    #[cfg(feature = "cef-engine")]
    pub(crate) fn load_engine_info_sizes(&mut self) -> Task<Message> {
        self.sync_home_engine_info();
        self.browser.engine_info_generation = self.browser.engine_info_generation.wrapping_add(1);
        let generation = self.browser.engine_info_generation;
        Task::perform(
            async move {
                super::spawn_blocking(|| {
                    // Measure the runtime the resolver actually uses, not just
                    // the local store (covers sidecar and MSIX/package dirs).
                    let size = crate::browser::cef_backend::resolve_engine_dir()
                        .or_else(crate::browser::cef_backend::loaded_engine_dir)
                        .map(|path| crate::browser::engine_install::dir_size(&path))
                        .unwrap_or(0);
                    let pending = crate::browser::engine_install::pending_cleanup_size();
                    (size, pending)
                })
                .await
            },
            move |result| Message::EngineInfoSize {
                generation,
                result: match result {
                    Ok(sizes) => Ok(sizes),
                    Err(err) => Err(err.to_string()),
                },
            },
        )
    }

    /// Refresh Home's engine version/update/changelog view from the local
    /// install metadata and the last fetched manifest.
    #[cfg(feature = "cef-engine")]
    pub(crate) fn sync_home_engine_info(&mut self) {
        let installed = crate::browser::engine_install::active_meta();
        let remote = self.browser.engine_manifest.clone();
        let installed_version = installed.as_ref().map(|(_, meta)| meta.version.clone());
        let managed = self.browser.engine_managed;
        let update_ready = !managed
            && remote.as_ref().is_some_and(|remote| {
                installed_version.as_deref().is_some_and(|current| {
                    crate::browser::engine_install::version_is_newer(&remote.version, current)
                })
            });
        let changelog = if update_ready {
            remote
                .as_ref()
                .map(|remote| remote.changelog.clone())
                .filter(|text| !text.trim().is_empty())
                .or_else(|| installed.as_ref().map(|(_, meta)| meta.changelog.clone()))
        } else {
            installed
                .as_ref()
                .map(|(_, meta)| meta.changelog.clone())
                .filter(|text| !text.trim().is_empty())
                .or_else(|| remote.as_ref().map(|remote| remote.changelog.clone()))
        };
        self.home_state.engine_version = if managed {
            remote
                .as_ref()
                .map(|remote| remote.version.clone())
                .or(installed_version)
        } else {
            installed_version
        };
        self.home_state.engine_latest_version =
            remote.as_ref().map(|remote| remote.version.clone());
        self.home_state.engine_changelog = changelog.filter(|text| !text.trim().is_empty());
        self.home_state.engine_size = installed
            .as_ref()
            .map(|(_, meta)| meta.size)
            .filter(|size| *size > 0);
        self.home_state.engine_update_ready = update_ready;
        self.home_state.engine_managed = managed;
        self.home_state.engine_restart_required = self.browser.engine_restart_required;
    }

    #[cfg(not(feature = "cef-engine"))]
    pub(crate) fn sync_home_engine_info(&mut self) {}

    #[cfg(feature = "cef-engine")]
    pub(crate) fn on_engine_install_progress(
        &mut self,
        generation: u64,
        progress: crate::browser::engine_install::InstallProgress,
    ) -> Task<Message> {
        if generation != self.browser.engine_install_generation
            || !matches!(self.browser.engine_status, EngineStatus::Installing)
            || self.home_state.cancelling
        {
            return Task::none();
        }
        let rate = self.note_engine_rate(&progress);
        let mut mapped = home_install_progress(progress);
        mapped.rate_bps = rate;
        self.home_state.set_install_progress(mapped);
        Task::none()
    }

    #[cfg(feature = "cef-engine")]
    fn note_engine_rate(
        &mut self,
        progress: &crate::browser::engine_install::InstallProgress,
    ) -> Option<f64> {
        use crate::browser::engine_install::InstallProgress as Engine;
        let done = match progress {
            Engine::Downloading { received, .. } => *received,
            Engine::Verifying { done, .. } | Engine::Extracting { done, .. } => *done,
            _ => {
                self.browser.engine_rate = None;
                return None;
            }
        };
        let now = Instant::now();
        let sample = match self.browser.engine_rate {
            Some(prev) if done >= prev.bytes => {
                let elapsed = now.saturating_duration_since(prev.at).as_secs_f64();
                if elapsed < 0.02 {
                    prev
                } else {
                    let instant = (done - prev.bytes) as f64 / elapsed;
                    EngineRateSample {
                        at: now,
                        bytes: done,
                        ewma_bps: prev.ewma_bps * 0.65 + instant * 0.35,
                    }
                }
            }
            _ => EngineRateSample {
                at: now,
                bytes: done,
                ewma_bps: 0.0,
            },
        };
        self.browser.engine_rate = Some(sample);
        (sample.ewma_bps > 0.0).then_some(sample.ewma_bps)
    }

    #[cfg(feature = "cef-engine")]
    pub(crate) fn on_engine_install_done(
        &mut self,
        generation: u64,
        outcome: EngineInstallOutcome,
    ) -> Task<Message> {
        if generation != self.browser.engine_install_generation
            || !matches!(self.browser.engine_status, EngineStatus::Installing)
        {
            return Task::none();
        }
        // Invalidate any progress messages still buffered behind the result.
        self.browser.engine_install_generation =
            self.browser.engine_install_generation.wrapping_add(1);
        self.browser.engine_rate = None;
        match outcome {
            EngineInstallOutcome::Installed(path) => {
                self.browser.engine_install_previous = None;
                self.browser.engine_status = EngineStatus::Ready(path.clone());
                self.browser.engine_available = true;
                self.browser.engine_managed = false;
                if self.browser.engine.is_some() {
                    // Update: CEF is process-global, so the old runtime stays
                    // loaded until restart. The new directory is selected by
                    // the pointer on the next boot.
                    self.browser.engine_restart_required =
                        crate::browser::cef_backend::loaded_engine_dir().as_deref()
                            != Some(path.as_path());
                } else {
                    self.refresh_engine_status();
                }
                if self.browser.engine.is_some() {
                    self.home_state.install_done(true, None);
                    crate::platform::app_log::info(format!(
                        "cef: BrowserEngine install complete{}",
                        if self.browser.engine_restart_required {
                            " (restart required)"
                        } else {
                            ""
                        }
                    ));
                } else {
                    let msg = "Browser engine installed but failed to start".to_string();
                    self.browser.engine_status = EngineStatus::Failed(msg.clone());
                    self.home_state.install_done(false, Some(msg));
                }
            }
            EngineInstallOutcome::Cancelled => {
                let previous = self.browser.engine_install_previous.take();
                let ready = previous.as_ref().is_some_and(EngineStatus::is_ready);
                self.browser.engine_status = previous.unwrap_or(EngineStatus::Missing);
                self.browser.engine_available = self.browser.engine.is_some();
                self.home_state.install_cancelled(ready);
                crate::platform::app_log::info("cef: BrowserEngine install cancelled");
            }
            EngineInstallOutcome::Failed(err) => {
                crate::platform::app_log::warn(format!("cef: BrowserEngine install failed: {err}"));
                let previous = self.browser.engine_install_previous.take();
                let ready = previous.as_ref().is_some_and(EngineStatus::is_ready);
                // A failed update keeps the working engine and offers Retry
                // instead of dimming the app to Missing.
                self.browser.engine_status = previous
                    .filter(|status| status.is_ready())
                    .unwrap_or_else(|| EngineStatus::Failed(err.clone()));
                self.home_state.install_done(ready, Some(err));
            }
        }
        self.sync_home_engine_info();
        Task::none()
    }

    #[cfg(feature = "cef-engine")]
    pub(crate) fn on_engine_uninstall_done(
        &mut self,
        generation: u64,
        result: Result<(), String>,
    ) -> Task<Message> {
        if generation != self.browser.engine_install_generation
            || !matches!(self.browser.engine_status, EngineStatus::Uninstalling)
        {
            return Task::none();
        }
        self.browser.engine_install_generation =
            self.browser.engine_install_generation.wrapping_add(1);
        match result {
            Ok(()) => {
                self.browser.engine_status = EngineStatus::Missing;
                self.browser.engine_available = false;
                self.browser.engine_managed = false;
                self.browser.engine_restart_required = false;
                self.home_state.uninstall_done(None);
                crate::platform::app_log::info("cef: BrowserEngine uninstalled");
            }
            Err(err) => {
                crate::platform::app_log::warn(format!("cef: uninstall failed: {err}"));
                self.browser.engine_status = EngineStatus::Failed(err.clone());
                self.home_state.uninstall_done(Some(err));
            }
        }
        self.sync_home_engine_info();
        Task::none()
    }

    #[cfg(not(feature = "cef-engine"))]
    pub(crate) fn on_engine_install_done(
        &mut self,
        _generation: u64,
        _outcome: EngineInstallOutcome,
    ) -> Task<Message> {
        Task::none()
    }
}

#[cfg(feature = "cef-engine")]
fn home_install_progress(
    progress: crate::browser::engine_install::InstallProgress,
) -> crate::ui::home::EngineProgress {
    use crate::browser::engine_install::InstallProgress as Engine;
    use crate::ui::home::{EnginePhase, EngineProgress};
    match progress {
        Engine::Starting => EngineProgress {
            phase: EnginePhase::Preparing,
            fraction: None,
            bytes_done: 0,
            bytes_total: None,
            rate_bps: None,
        },
        Engine::Downloading { received, total } => EngineProgress {
            phase: EnginePhase::Downloading,
            fraction: total
                .filter(|total| *total > 0)
                .map(|total| received as f32 / total as f32),
            bytes_done: received,
            bytes_total: total,
            rate_bps: None,
        },
        Engine::Verifying { done, total } => EngineProgress {
            phase: EnginePhase::Verifying,
            fraction: (total > 0).then(|| done as f32 / total as f32),
            bytes_done: done,
            bytes_total: Some(total),
            rate_bps: None,
        },
        Engine::Extracting { done, total } => EngineProgress {
            phase: EnginePhase::Installing,
            fraction: (total > 0).then(|| done as f32 / total as f32),
            bytes_done: done,
            bytes_total: Some(total),
            rate_bps: None,
        },
        Engine::Finishing => EngineProgress {
            phase: EnginePhase::Finishing,
            fraction: None,
            bytes_done: 0,
            bytes_total: None,
            rate_bps: None,
        },
    }
}

/// Replay key modifiers from a JSON array (`["ctrl","shift"]`). The keys are
/// only constructible by the debug replay driver.
#[cfg(debug_assertions)]
fn replay_modifiers(value: Option<&serde_json::Value>) -> iced::keyboard::Modifiers {
    use iced::keyboard::Modifiers;
    let mut modifiers = Modifiers::empty();
    let Some(items) = value.and_then(serde_json::Value::as_array) else {
        return modifiers;
    };
    for item in items {
        match item.as_str() {
            Some("ctrl" | "control") => modifiers |= Modifiers::CTRL,
            Some("shift") => modifiers |= Modifiers::SHIFT,
            Some("alt") => modifiers |= Modifiers::ALT,
            Some("meta" | "logo" | "super") => modifiers |= Modifiers::LOGO,
            _ => {}
        }
    }
    modifiers
}

/// Replay page-local point (`{"x":..,"y":..}`).
#[cfg(debug_assertions)]
fn replay_point(params: &serde_json::Value) -> Option<(f32, f32)> {
    let x = params.get("x")?.as_f64()? as f32;
    let y = params.get("y")?.as_f64()? as f32;
    Some((x, y))
}

/// Bounded browser state for action traces. Includes the UI stack, the last
/// raw engine history, CEF's back/forward flags, and the selector list so a
/// picked index can always be resolved back to an actual URL/title.
pub(crate) fn trace_browser_state(
    panel: &crate::ui::browser::state::BrowserPanel,
) -> serde_json::Value {
    use serde_json::json;

    fn row(index: usize, entry: &crate::browser::HistoryEntry) -> serde_json::Value {
        json!({
            "index": index,
            "url": &entry.url,
            "title": &entry.title,
        })
    }

    fn nav_window(
        entries: &[crate::browser::HistoryEntry],
        current: Option<usize>,
    ) -> serde_json::Value {
        let center = current.unwrap_or(0).min(entries.len().saturating_sub(1));
        let start = center.saturating_sub(8);
        let end = (center + 9).min(entries.len());
        let rows: Vec<serde_json::Value> = entries[start..end]
            .iter()
            .enumerate()
            .map(|(offset, entry)| row(start + offset, entry))
            .collect();
        json!({
            "current": current,
            "len": entries.len(),
            "from": start,
            "entries": rows,
        })
    }

    let page = &panel.page;
    let session = &page.session;
    let engine_history = page.last_engine_history.as_ref().map(|history| {
        let center = history.current.min(history.entries.len().saturating_sub(1));
        let start = center.saturating_sub(8);
        let end = (center + 9).min(history.entries.len());
        let rows: Vec<serde_json::Value> = history.entries[start..end]
            .iter()
            .enumerate()
            .map(|(offset, entry)| {
                json!({
                    "index": start + offset,
                    "url": &entry.url,
                    "original_url": &entry.original_url,
                    "title": &entry.title,
                })
            })
            .collect();
        json!({
            "current": history.current,
            "len": history.entries.len(),
            "from": start,
            "entries": rows,
        })
    });

    json!({
        "ui": {
            "tab": panel.active_tab.0,
            "tabs": panel.order.len(),
            "url": session.url(),
            "title": session.title(),
            "loading": session.loading(),
            "generation": session.generation(),
            "url_editing": panel.url_editing,
            "url_draft_len": panel.url_draft.chars().count(),
            "grid_mode": format!("{:?}", panel.grid.mode),
            "grid_focus": format!("{:?}", panel.grid.focus),
            "grid_cards": panel.grid.visual.len(),
            "bookmarks": panel.bookmarks.len(),
            "nav": nav_window(session.nav_entries(), session.nav_current()),
        },
        "engine": {
            "history": engine_history,
            "can_back": page.last_engine_can.map(|(back, _)| back),
            "can_forward": page.last_engine_can.map(|(_, forward)| forward),
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::pad::PadButtonAction;

    fn tap(button: PadButton) -> PadAction {
        PadAction {
            pad: PadId("hid:a".into()),
            button,
            action: PadButtonAction::Tap,
        }
    }

    fn hold(button: PadButton) -> PadAction {
        PadAction {
            pad: PadId("hid:a".into()),
            button,
            action: PadButtonAction::Hold,
        }
    }

    #[test]
    fn grid_nav_uses_left_stick_with_menu_deadzone() {
        use crate::domain::pad::{PadSample, hid_nav_reading};
        let light = hid_nav_reading(
            "hid:a",
            PadSample {
                lstick_x: 0.5,
                ..PadSample::default()
            },
        );
        let shaped = grid_nav_readings(&[light]);
        assert_eq!(
            shaped[0].sample.stick_x, 0.0,
            "a light left-stick touch must not move the tabs view"
        );

        let firm = hid_nav_reading(
            "hid:a",
            PadSample {
                lstick_x: 0.95,
                ..PadSample::default()
            },
        );
        let shaped = grid_nav_readings(&[firm]);
        assert!(
            shaped[0].sample.stick_x >= 0.9,
            "a full left-stick deflection must pass the menu deadzone"
        );

        let right_only = hid_nav_reading(
            "hid:a",
            PadSample {
                rstick_x: 1.0,
                dpad_left: true,
                ..PadSample::default()
            },
        );
        let shaped = grid_nav_readings(&[right_only]);
        assert_eq!(
            shaped[0].sample.stick_x, 0.0,
            "the right stick scrolls bookmarks, never card focus"
        );
        assert!(shaped[0].sample.dpad_left, "D-pad still drives focus");
    }

    #[test]
    fn trace_state_exposes_nav_entries() {
        let mut panel = crate::ui::browser::state::BrowserPanel::default();
        panel.session.open_surface();
        panel.session.begin_load("https://b.test/".to_string());
        panel.session.apply_engine_history(
            &[
                crate::browser::engine::EngineHistoryEntry {
                    url: "https://a.test/".to_string(),
                    display_url: "https://a.test/".to_string(),
                    original_url: "https://a.test/".to_string(),
                    title: Some("A".to_string()),
                    has_post_data: false,
                },
                crate::browser::engine::EngineHistoryEntry {
                    url: "https://b.test/".to_string(),
                    display_url: "https://b.test/".to_string(),
                    original_url: "https://b.test/".to_string(),
                    title: Some("B".to_string()),
                    has_post_data: false,
                },
            ],
            1,
        );
        let state = trace_browser_state(&panel);
        assert_eq!(state["ui"]["nav"]["current"], 1);
        assert_eq!(state["ui"]["nav"]["entries"][0]["url"], "https://a.test/");
        assert!(state["ui"].get("selector").is_none());
        assert_eq!(state["ui"]["url_editing"], false);
        assert_eq!(state["ui"]["url_draft_len"], 0);

        panel.start_url_edit();
        panel.url_edit_insert("https://c.test/");
        let state = trace_browser_state(&panel);
        assert_eq!(state["ui"]["url_editing"], true);
        assert_eq!(state["ui"]["url_draft_len"], "https://c.test/".len());
    }

    #[test]
    fn browser_taps_map_to_generic_nav_actions() {
        for (button, expected) in [
            (PadButton::Cross, NavAction::Confirm),
            (PadButton::Circle, NavAction::Cancel),
            (PadButton::Triangle, NavAction::Triangle),
            (PadButton::L3, NavAction::ToggleMagnetism),
            (PadButton::Create, NavAction::ToggleControllers),
        ] {
            assert_eq!(
                browser_tap_action(&[tap(button)]).map(|(_, action)| action),
                Some(expected)
            );
        }
        // Semantic bindings (L1/R1/Options) are resolved by the OS action
        // table, not this generic map.
        for button in [PadButton::L1, PadButton::R1, PadButton::Options] {
            assert!(browser_tap_action(&[tap(button)]).is_none());
        }
        // Tap only: holds never invoke actions.
        assert!(browser_tap_action(&[hold(PadButton::L1)]).is_none());
        assert!(browser_tap_action(&[hold(PadButton::R1)]).is_none());
        assert!(browser_tap_action(&[tap(PadButton::R3)]).is_none());
        assert!(browser_tap_action(&[hold(PadButton::Cross)]).is_none());
    }

    #[cfg(debug_assertions)]
    #[test]
    fn replay_input_helpers_parse_points_and_modifiers() {
        assert_eq!(
            replay_point(&serde_json::json!({"x": 12.5, "y": 40})),
            Some((12.5, 40.0))
        );
        assert_eq!(replay_point(&serde_json::json!({"x": 1})), None);
        let modifiers = replay_modifiers(Some(&serde_json::json!(["ctrl", "shift", "bogus"])));
        assert!(modifiers.control());
        assert!(modifiers.shift());
        assert!(!modifiers.alt());
    }

    #[test]
    fn click_chain_counts_double_triple_and_resets() {
        let now = Instant::now();
        let (first, stamp) = next_click_count(None, now, 10.0, 20.0);
        assert_eq!(first, 1);
        let (second, stamp) = next_click_count(
            stamp,
            now + std::time::Duration::from_millis(100),
            11.0,
            20.0,
        );
        assert_eq!(second, 2);
        let (third, stamp) = next_click_count(
            stamp,
            now + std::time::Duration::from_millis(200),
            10.0,
            20.0,
        );
        assert_eq!(third, 3);
        let (capped, _) = next_click_count(
            stamp,
            now + std::time::Duration::from_millis(300),
            10.0,
            20.0,
        );
        assert_eq!(capped, 3);
        let (moved, _) = next_click_count(
            Some((now, 10.0, 20.0, 1)),
            now + std::time::Duration::from_millis(100),
            20.0,
            20.0,
        );
        assert_eq!(moved, 1);
        let (late, _) = next_click_count(
            Some((now, 10.0, 20.0, 1)),
            now + std::time::Duration::from_millis(600),
            10.0,
            20.0,
        );
        assert_eq!(late, 1);
    }

    /// NAV-1 close matrix: every close path owns exactly one cue.
    #[test]
    fn tab_close_matrix_emits_exactly_one_cue() {
        use crate::app::App;
        use crate::browser::engine::TabId;

        fn cues(panel: &BrowserPanel, id: TabId) -> Vec<UiSoundKind> {
            App::close_tab_cue(panel.tab_close_outcome(id))
                .into_iter()
                .collect()
        }

        // Final remaining tab: the reset navigation is silent, the close owns
        // the single Nav cue.
        let panel = BrowserPanel::default();
        assert_eq!(cues(&panel, TabId::BOOT), vec![UiSoundKind::Nav]);

        // Active non-final and background closes: one Action each.
        let mut panel = BrowserPanel::default();
        let (active, _) = panel.create_tab("https://b.test/".to_string());
        assert_eq!(
            cues(&panel, active),
            vec![UiSoundKind::Action],
            "closing the active non-final tab"
        );
        assert_eq!(
            cues(&panel, TabId::BOOT),
            vec![UiSoundKind::Action],
            "closing a background tab"
        );

        // Unknown tab: rejected, silent.
        assert_eq!(cues(&panel, TabId(999)), Vec::<UiSoundKind>::new());
    }
}
