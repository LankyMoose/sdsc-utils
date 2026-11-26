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
    /// First fresh Browser surface attempted the persisted-strip restore.
    /// The engine preloads the saved active URL at boot, so "session URL is
    /// empty" is not a usable freshness test on later opens.
    pub tabs_restored: bool,
}

use iced::Task;
use std::time::Instant;

use super::Message;

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

const REPLAY_BROWSER_WAIT: std::time::Duration = std::time::Duration::from_millis(2500);
const REPLAY_STEP_WAIT: std::time::Duration = std::time::Duration::from_millis(1500);
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
    pub(crate) fn on_replay_step(&mut self) -> Task<Message> {
        let Some(plan) = &self.replay else {
            return Task::none();
        };
        let flow = plan.flow();
        let wants_browser = plan.peek_app() == Some("browser");
        if wants_browser && !self.browser_visible {
            let open = self.open_browser_window();
            return Task::batch([
                open,
                Task::perform(super::delay(REPLAY_BROWSER_WAIT), |()| Message::ReplayStep),
            ]);
        }
        if self.pending_history_travel.is_some() || self.pending_browser_trace.is_some() {
            return Task::perform(super::delay(REPLAY_STEP_WAIT), |()| Message::ReplayStep);
        }
        let Some(step) = self.replay.as_mut().and_then(|plan| plan.next_step()) else {
            self.replay = None;
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
        let task = self.replay_dispatch(&step);
        crate::platform::trace::replay::set_replaying(false);
        Task::batch([
            task,
            Task::perform(super::delay(REPLAY_STEP_WAIT), |()| Message::ReplayStep),
        ])
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
                    _ => return Task::none(),
                };
                self.replay_pad_tap(button)
            }
            #[cfg(debug_assertions)]
            "pad.dir" => {
                let action = match step.params.get("dir").and_then(serde_json::Value::as_str) {
                    Some("up") => NavAction::Up,
                    Some("down") => NavAction::Down,
                    _ => return Task::none(),
                };
                self.replay_pad_dir(action)
            }
            #[cfg(debug_assertions)]
            "osk.stick" => {
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
                let pair = (x, y);
                match step.params.get("side").and_then(serde_json::Value::as_str) {
                    Some("right") => self.replay_osk_stick((0.0, 0.0), pair),
                    _ => self.replay_osk_stick(pair, (0.0, 0.0)),
                }
            }
            #[cfg(debug_assertions)]
            "osk.trigger" => {
                let side = step
                    .params
                    .get("side")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("left");
                self.replay_osk_trigger(side)
            }
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
        if self.browser_visible
            && let Some(id) = self.browser_window
        {
            return crate::ui::layout::focus_window(id);
        }
        if self.browser_window.is_some() {
            crate::controller::hid::diag::diag_info(
                "ui-diag: browser open skipped (window still closing)",
            );
            return Task::none();
        }
        if self.browser.engine.is_none() {
            crate::controller::hid::diag::diag_info(
                "ui-diag: browser open skipped (engine not installed)",
            );
            return Task::none();
        }
        // Launcher and Browser are mutually exclusive: opening one dismisses
        // the other rather than stacking two fullscreen apps.
        let close_start = if self.launcher_visible {
            self.close_launcher()
        } else {
            Task::none()
        };
        let cover =
            crate::ui::layout::primary_monitor_cover().unwrap_or(crate::ui::layout::MonitorCover {
                x: 0.0,
                y: 0.0,
                width: 1920.0,
                height: 1080.0,
            });
        self.browser_monitor_cover = Some(cover);
        // A new window can land on a different monitor; the cover drives the
        // first layout until its own resize event arrives.
        self.browser_window_size = None;
        self.browser.panel.l1_held = false;
        self.browser.panel.r1_held = false;
        self.browser.panel.triangle_held = false;
        self.sync_browser_viewport();
        // Center before the first frame, not after the window has painted the
        // previous open's cursor position.
        self.browser.panel.cursor.reset_for_page();
        self.browser_visible = true;
        self.browser_pending_show = true;
        let was_open = self.browser.panel.session.is_open();
        let (id, open) = iced::window::open(iced::window::Settings {
            size: cover.size(),
            position: iced::window::Position::Specific(cover.origin()),
            // Hidden until the final cover size is configured and DWM
            // transitions are disabled; showing earlier lets Windows animate
            // the create-time default size into the full-screen rect.
            visible: false,
            // The 16:9 card sits over a dimmed desktop; the root view draws
            // the scrim on backends that can composite alpha.
            transparent: crate::ui::chrome::transparent_windows(),
            resizable: false,
            decorations: false,
            level: iced::window::Level::Normal,
            exit_on_close_request: true,
            platform_specific: crate::ui::layout::window_platform_specific(),
            ..iced::window::Settings::default()
        });
        self.browser_window = Some(id);
        self.arm_chord_release_latch();
        crate::controller::hid::diag::diag_info("ui-diag: browser window open");
        let opened = open.map(Message::BrowserOpened);
        let opened = if was_open {
            // Reopen after close: cached page, suspended engine.
            self.browser.panel.session.open_surface();
            self.resume_browser_engine();
            opened
        } else {
            // Fresh surface: restore the saved page or open the default page.
            opened.chain(self.open_browser_session())
        };
        close_start
            .chain(crate::ui::layout::disable_window_transitions(id))
            .chain(opened)
            .chain(Task::done(Message::WindowShowNow(id)))
    }

    /// Close the standalone Browser window and suspend its engine (cached
    /// page survives for the next open).
    pub(crate) fn close_browser_window(&mut self) -> Task<Message> {
        self.browser_visible = false;
        self.browser_monitor_cover = None;
        self.browser_window_size = None;
        self.browser_pending_show = false;
        self.browser_backdrop_fade = None;
        let task = self.close_osk("browser_close");
        let task = task.chain(self.close_browser_surface());
        let Some(id) = self.browser_window.take() else {
            return task;
        };
        crate::controller::hid::diag::diag_info("ui-diag: browser window close");
        let close = iced::window::close(id);
        let task = task.chain(close);
        if self.launcher_visible
            && let Some(start) = self.launcher_window
        {
            return task.chain(crate::ui::layout::focus_window(start));
        }
        task
    }

    pub(crate) fn on_browser_opened(&mut self, id: iced::window::Id) -> Task<Message> {
        if self.browser_window != Some(id) {
            return Task::none();
        }
        self.browser_visible = true;
        crate::controller::hid::diag::diag_info(format!("ui-diag: browser opened id={id:?}"));
        // The cover is logical pixels and already drives this first layout.
        // The real size arrives through `window::resize_events()` once the
        // fullscreen surface is configured; a one-shot `window::size()` here
        // can race creation and report the pre-fullscreen default, which
        // would briefly shrink the cursor space (the quarter-screen bug).
        self.sync_browser_viewport();
        Task::none()
    }

    /// Show the hidden Browser window after the DWM transition task has run
    /// and the surface has had a moment to settle at the cover size. Showing
    /// immediately lets Windows animate the create-time default size into the
    /// fullscreen rect (the visible "scale" on open).
    pub(crate) fn on_browser_show_now(&mut self) -> Task<Message> {
        if !self.browser_pending_show {
            return Task::none();
        }
        self.browser_pending_show = false;
        let Some(id) = self.browser_window else {
            return Task::none();
        };
        self.browser_backdrop_fade = Some(Instant::now());
        self.sync_browser_viewport();
        crate::ui::layout::show_window(id).chain(crate::ui::layout::focus_window(id))
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
        // PS is global: while Browser owns the pad it must still open Home
        // (Home-open path routes PS to close once visible).
        if self.home_chord_fires(readings) {
            crate::controller::hid::diag::diag_info("ui-diag: PS opens home over Browser");
            return self.toggle_home();
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

    /// Escape cancels URL editing but never closes the Browser.
    pub(crate) fn browser_cancel(&mut self) -> Task<Message> {
        if self.browser.panel.url_editing {
            return self.cancel_browser_url_edit();
        }
        Task::none()
    }

    /// Circle: cancel URL editing when it owns the pad, else suspend and close
    /// the Browser window. Home/PS remains the overlay/close path from afar.
    pub(crate) fn on_browser_circle(&mut self) -> Task<Message> {
        if self.browser.panel.url_editing {
            return self.cancel_browser_url_edit();
        }
        crate::platform::trace::event(crate::platform::trace::Lane::Browser, "browser.close")
            .field("source", "circle")
            .emit();
        self.close_browser_window()
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
}
