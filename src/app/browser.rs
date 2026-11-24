//! Top-level Browser app state.
//!
//! Phase 0 of Home pivot lifts the browser out of `LauncherState`: the panel,
//! engine handle, capability flag, and browser-only D-pad stepper live here so
//! the browser no longer disappears when the Launcher is not the owner.
//! Behavior is unchanged — Launcher still renders the panel until the standalone
//! Browser window lands. See `notes/browser-Home-pivot.md`.

use crate::browser::engine::{EngineCommand, EngineHandle};
use crate::domain::pad::{NavAction, NavStepper, PadAction, PadButton, PadButtonAction, PadId};
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

/// A history jump that needs one CEF step per canonical snapshot. CEF has no
/// goto-index, and `GoBack`/`GoForward` are asynchronous one-entry moves.
pub(crate) struct PendingHistoryTravel {
    pub tab: crate::browser::engine::TabId,
    pub target_index: usize,
    pub target_url: Option<String>,
    pub attempts: u8,
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
    /// Accumulated analog scroll for the next BrowserFrame tick.
    pub pending_scroll: (f32, f32),
    /// Page point the pending scroll belongs to (cursor at accumulation).
    pub pending_scroll_at: (f32, f32),
    /// Engine-mode D-pad cursor jumps run on this standalone stepper (fed a
    /// stickless `dpad_only` mask): the Launcher bank stays parked in engine
    /// mode so stick moves never double-fire into both cursor and nav.
    pub dpad_stepper: NavStepper,
    /// Browser-window pad bank: face/bumper edges + reader nav. The engine
    /// cursor uses `dpad_stepper` and raw stick samples instead.
    pub nav: crate::domain::pad::PadNavBank,
    /// First fresh Browser surface attempted the persisted-strip restore.
    /// The engine preloads the saved active URL at boot, so "session URL is
    /// empty" is not a usable freshness test on later opens.
    pub tabs_restored: bool,
}

use iced::Task;
use std::time::Instant;

use super::Message;

/// Map source-side tap actions to the Browser's semantic nav actions.
fn browser_tap_action(actions: &[PadAction]) -> Option<(PadId, NavAction)> {
    actions.iter().find_map(|action| {
        let nav = match (action.button, action.action) {
            (PadButton::Cross, PadButtonAction::Tap) => NavAction::Confirm,
            (PadButton::Circle, PadButtonAction::Tap) => NavAction::Cancel,
            (PadButton::Square, PadButtonAction::Tap) => NavAction::CycleSort,
            (PadButton::Triangle, PadButtonAction::Tap) => NavAction::Triangle,
            (PadButton::Options, PadButtonAction::Tap) => NavAction::ToggleSettings,
            (PadButton::Create, PadButtonAction::Tap) => NavAction::ToggleControllers,
            (PadButton::L1, PadButtonAction::Tap) => NavAction::BumperLeft,
            (PadButton::R1, PadButtonAction::Tap) => NavAction::BumperRight,
            (PadButton::L1, PadButtonAction::Hold) => NavAction::BumperHoldLeft,
            (PadButton::R1, PadButtonAction::Hold) => NavAction::BumperHoldRight,
            (PadButton::L3, PadButtonAction::Tap) => NavAction::ToggleMagnetism,
            _ => return None,
        };
        Some((action.pad.clone(), nav))
    })
}

const REPLAY_BROWSER_WAIT: std::time::Duration = std::time::Duration::from_millis(2500);
const REPLAY_STEP_WAIT: std::time::Duration = std::time::Duration::from_millis(1500);

impl super::App {
    /// Replay driver tick. Opens the Browser surface first, validates stable
    /// preconditions, then dispatches the next semantic step through the same
    /// handlers a real action uses.
    pub(crate) fn on_replay_step(&mut self) -> Task<Message> {
        let Some(plan) = &self.replay else {
            return Task::none();
        };
        let flow = plan.flow();
        if !self.browser_visible {
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
            return Task::none();
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
                self.browser.panel.draft = url.to_string();
                self.on_browser_navigate()
            }
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
            "history.open" => {
                let back = step
                    .params
                    .get("back")
                    .and_then(serde_json::Value::as_bool)
                    .unwrap_or(true);
                self.on_browser_history_open(back)
            }
            "history.pick" => {
                let requested = step
                    .params
                    .get("index")
                    .and_then(serde_json::Value::as_u64)
                    .map(|index| index as usize);
                let target_url = step.params.get("url").and_then(serde_json::Value::as_str);
                // Resolve by URL first: indices shift when the stack differs.
                let resolved = target_url
                    .and_then(|url| {
                        self.browser
                            .panel
                            .session
                            .nav_entries()
                            .iter()
                            .position(|entry| entry.url == url)
                    })
                    .or(requested);
                match resolved {
                    Some(index) => self.on_browser_history_pick(index),
                    None => Task::none(),
                }
            }
            "history.cancel" => self.on_browser_history_cancel(),
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

    /// Begin a history action and send its first one-step traversal. CEF's
    /// canonical snapshot advances the rest of a multi-step jump.
    pub(crate) fn start_history_travel(
        &mut self,
        action: &'static str,
        source: &'static str,
        target_index: usize,
        target_url: Option<String>,
    ) {
        self.begin_browser_action_trace(action, source, Some(target_index), target_url.clone());
        let tab = self.browser.panel.active_tab;
        self.pending_history_travel = Some(crate::app::browser::PendingHistoryTravel {
            tab,
            target_index,
            target_url,
            attempts: 0,
        });
        self.send_history_step(tab, target_index);
    }

    /// Advance an in-flight history jump after a canonical snapshot. One CEF
    /// step per snapshot avoids the N-synchronous-GoBack race.
    pub(crate) fn advance_history_travel(&mut self, tab: crate::browser::engine::TabId) {
        let Some((pending_tab, target_index, target_url, attempts)) =
            self.pending_history_travel.as_ref().map(|pending| {
                (
                    pending.tab,
                    pending.target_index,
                    pending.target_url.clone(),
                    pending.attempts,
                )
            })
        else {
            return;
        };
        if pending_tab != tab {
            return;
        }
        if self.browser.panel.session.nav_current() == Some(target_index) {
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
                && self.browser.panel.session.url() != expected
            {
                return;
            }
            self.pending_history_travel = None;
            self.settle_browser_action_trace("settled");
            return;
        }
        if attempts >= 64 {
            self.pending_history_travel = None;
            self.settle_browser_action_trace("stalled");
            return;
        }
        if let Some(pending) = self.pending_history_travel.as_mut() {
            pending.attempts = pending.attempts.saturating_add(1);
        }
        self.send_history_step(tab, target_index);
    }

    /// Close a history travel when the address change confirms the target.
    /// CEF can report the new history index before `OnAddressChange`; settling
    /// on the index alone leaves the trace on the previous page's URL.
    pub(crate) fn settle_history_travel_on_url(&mut self, tab: crate::browser::engine::TabId) {
        let Some((pending_tab, target_index, target_url)) =
            self.pending_history_travel.as_ref().map(|pending| {
                (
                    pending.tab,
                    pending.target_index,
                    pending.target_url.clone(),
                )
            })
        else {
            return;
        };
        if pending_tab != tab || self.browser.panel.session.nav_current() != Some(target_index) {
            return;
        }
        if let Some(expected) = target_url
            && self.browser.panel.session.url() != expected
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
        let Some((pending_tab, target_index)) = self
            .pending_history_travel
            .as_ref()
            .map(|pending| (pending.tab, pending.target_index))
        else {
            return;
        };
        if pending_tab != tab || self.browser.panel.session.nav_current() != Some(target_index) {
            return;
        }
        self.pending_history_travel = None;
        self.settle_browser_action_trace("settled");
    }

    /// Send one CEF history step toward `target_index`. The next canonical
    /// snapshot calls `advance_history_travel` again.
    pub(crate) fn send_history_step(
        &mut self,
        tab: crate::browser::engine::TabId,
        target_index: usize,
    ) {
        let Some(current) = self.browser.panel.session.nav_current() else {
            self.pending_history_travel = None;
            self.settle_browser_action_trace("history_missing");
            return;
        };
        if current == target_index {
            self.pending_history_travel = None;
            self.settle_browser_action_trace("settled");
            return;
        }
        let back = current > target_index;
        let flow = self
            .pending_browser_trace
            .as_ref()
            .map(|pending| pending.action.flow());
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
            BrowserMessage::Close => self.close_browser_window(),
            BrowserMessage::UrlChanged(draft) => {
                self.browser.panel.draft = draft;
                Task::none()
            }
            BrowserMessage::ToggleUrlEntry => self.on_toggle_url_entry(),
            BrowserMessage::Navigate => self.on_browser_navigate(),
            BrowserMessage::AutocompletePick(url) => {
                self.browser.panel.draft = url;
                self.on_browser_navigate()
            }
            BrowserMessage::Back => self.on_browser_go_back(),
            BrowserMessage::Forward => self.on_browser_go_forward(),
            BrowserMessage::HistoryOpen { back } => self.on_browser_history_open(back),
            BrowserMessage::HistoryPick(index) => self.on_browser_history_pick(index),
            BrowserMessage::HistoryCancel => self.on_browser_history_cancel(),
            BrowserMessage::OpenExternal => self.on_browser_open_external(),
            BrowserMessage::Reload => self.on_browser_reload(),
            BrowserMessage::DefaultPage => self.on_browser_go_default(),
            BrowserMessage::TabNew => self.on_browser_tab_new(),
            BrowserMessage::TabNext => self.on_browser_tab_next(),
            BrowserMessage::TabSwitch(id) => self.on_browser_tab_switch(id),
            BrowserMessage::TabClose(id) => self.on_browser_tab_close(id),
            BrowserMessage::BookmarkStar => self.on_browser_bookmark_star(),
            BrowserMessage::BookmarkNameChanged(draft) => {
                self.browser.panel.bookmark_name_draft = draft;
                Task::none()
            }
            BrowserMessage::BookmarkSave => self.on_browser_bookmark_save(),
            BrowserMessage::BookmarkRemove => self.on_browser_bookmark_remove(),
            BrowserMessage::Scrolled(y) => {
                self.browser.panel.note_scrolled(y);
                Task::none()
            }
            #[cfg(debug_assertions)]
            BrowserMessage::Shot(shot) => self.save_browser_shot(shot),
        }
    }
}

impl super::App {
    /// Show the standalone Browser window (idempotent). Restores the persisted
    /// tab strip on a fresh session, else resumes the cached page.
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
        self.sync_browser_viewport();
        // Center before the first frame, not after the window has painted the
        // previous open's cursor position.
        self.browser.panel.cursor.reset_for_page();
        self.browser.panel.last_sent_mouse = None;
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
        // Fetch conventional bookmark icons once the engine and the UI event
        // subscription are alive; restored bookmarks would otherwise stay
        // iconless until each site is visited.
        self.prefetch_bookmark_favicons();
        crate::controller::hid::diag::diag_info("ui-diag: browser window open");
        let opened = open.map(Message::BrowserOpened);
        let opened = if was_open {
            // Reopen after close: cached page, suspended engine.
            self.browser.panel.session.open_surface();
            self.resume_browser_engine();
            opened
        } else {
            // Fresh surface: open the tab strip (restore) or the default page.
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
        let task = self.close_browser_surface();
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

    /// Ask the engine for `/favicon.ico` for remembered sites that have no
    /// cached icon yet. Space is bounded and origins are deduped; the worker
    /// also dedupes across opens.
    fn prefetch_bookmark_favicons(&mut self) {
        let Some(engine) = &self.browser.engine else {
            return;
        };
        let mut seen = std::collections::HashSet::new();
        for bookmark in self.browser.panel.bookmarks.iter().take(32) {
            let key = crate::browser::bookmarks::origin_key(&bookmark.url);
            if key == bookmark.url
                || self.browser.panel.favicons.contains_key(&key)
                || !seen.insert(key)
            {
                continue;
            }
            engine.send(crate::browser::engine::EngineCommand::PrefetchFavicon {
                url: bookmark.url.clone(),
            });
        }
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

    pub(crate) fn on_browser_frame(&mut self) -> Task<Message> {
        if !self.browser_visible {
            return Task::none();
        }
        // Cursor glides and script-driven scroll still need one mouse sync
        // per paint even when no new pad edge arrived.
        if let Some(engine) = &self.browser.engine {
            let (dx, dy) = std::mem::take(&mut self.browser.pending_scroll);
            if dx != 0.0 || dy != 0.0 {
                let (x, y) = self.browser.pending_scroll_at;
                engine.send(EngineCommand::ScrollBy {
                    tab: self.browser_tab(),
                    dx,
                    dy,
                    x,
                    y,
                });
            }
        }
        let sync = self.sync_browser_mouse();
        #[cfg(debug_assertions)]
        {
            let shot = self.shoot_browser_window(false);
            sync.chain(shot)
        }
        #[cfg(not(debug_assertions))]
        {
            sync
        }
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
        if readings.is_empty() {
            return Task::none();
        }
        // PS is global: while Browser owns the pad it must still open Home
        // (Home-open path routes PS to close once visible).
        if self.home_chord_fires(readings) {
            crate::controller::hid::diag::diag_info("ui-diag: PS opens home over Browser");
            return self.toggle_home();
        }
        let now = std::time::Instant::now();
        let history_open = self.browser.panel.history_selector.is_some();
        let engine_mode = self.browser.panel.session.has_frame();
        // History selector uses D-pad nav; engine mode parks it (cursor
        // jumps run on the dedicated stepper).
        let allow_nav = history_open || !engine_mode;
        let mut tick = self.browser.nav.tick(
            readings, now, allow_nav, true, false, false, false, false, false,
        );
        // Source-side taps replace the old per-surface face-edge reconstruction.
        if let Some((pad, action)) = browser_tap_action(tap_actions) {
            // A hold that started before a URL/bookmark surface opened must
            // not pop the history selector over it.
            let hold_blocked = matches!(
                action,
                NavAction::BumperHoldLeft | NavAction::BumperHoldRight
            ) && (self.browser.panel.url_editing
                || self.browser.panel.bookmark_bubble);
            if !hold_blocked {
                tick.action = Some(action);
                tick.action_pad = Some(pad);
            }
        }
        self.set_haptic_pad_from_id(
            tick.action_pad
                .as_ref()
                .or(tick.cross_completed_pad.as_ref())
                .or(tick.triangle_completed_pad.as_ref()),
        );
        if history_open {
            return self.tick_browser_history_selector(readings, tick.action);
        }
        if engine_mode {
            self.tick_browser_cursor(readings, now);
            if let Some(jump) = self.poll_browser_dpad_jump(readings, now) {
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
            let action = match tick.action {
                Some(NavAction::Up) => {
                    self.play_ui_cue(UiSoundKind::Nav);
                    self.browser.panel.jump_cursor(JumpDir::Up);
                    Task::none()
                }
                Some(NavAction::Down) => {
                    self.play_ui_cue(UiSoundKind::Nav);
                    self.browser.panel.jump_cursor(JumpDir::Down);
                    Task::none()
                }
                Some(NavAction::Left) => {
                    self.play_ui_cue(UiSoundKind::Nav);
                    self.browser.panel.jump_cursor(JumpDir::Left);
                    Task::none()
                }
                Some(NavAction::Right) => {
                    self.play_ui_cue(UiSoundKind::Nav);
                    self.browser.panel.jump_cursor(JumpDir::Right);
                    Task::none()
                }
                Some(NavAction::Confirm) => {
                    if self.browser.panel.url_editing {
                        self.on_browser_message(crate::ui::browser::BrowserMessage::Navigate)
                    } else {
                        self.click_browser_cursor()
                    }
                }
                Some(NavAction::Cancel) => self.browser_cancel(),
                Some(NavAction::BumperLeft) => {
                    self.on_browser_message(crate::ui::browser::BrowserMessage::Back)
                }
                Some(NavAction::BumperRight) => {
                    self.on_browser_message(crate::ui::browser::BrowserMessage::Forward)
                }
                Some(NavAction::BumperHoldLeft) => {
                    self.on_browser_message(crate::ui::browser::BrowserMessage::HistoryOpen {
                        back: true,
                    })
                }
                Some(NavAction::BumperHoldRight) => {
                    self.on_browser_message(crate::ui::browser::BrowserMessage::HistoryOpen {
                        back: false,
                    })
                }
                Some(NavAction::Triangle) => {
                    self.on_browser_message(crate::ui::browser::BrowserMessage::ToggleUrlEntry)
                }
                Some(NavAction::ToggleMagnetism) => {
                    self.play_ui_cue(UiSoundKind::Action);
                    let cursor = &mut self.browser.panel.cursor;
                    cursor.magnet = !cursor.magnet;
                    if !cursor.magnet {
                        cursor.attached = None;
                        cursor.no_dock = None;
                    }
                    crate::platform::app_log::info(format!(
                        "ui-diag: browser magnet {}",
                        if cursor.magnet { "on" } else { "off" }
                    ));
                    Task::none()
                }
                _ => Task::none(),
            };
            let sync = self.sync_browser_mouse();
            Task::batch(vec![flag, action, sync])
        } else {
            // Reader fallback: D-pad scrolls text, Cross navigates.
            match tick.action {
                Some(NavAction::Up) | Some(NavAction::Left) => {
                    self.play_ui_cue(UiSoundKind::Nav);
                    self.scroll_browser(-crate::ui::browser::view::BROWSER_SCROLL_STEP)
                }
                Some(NavAction::Down) | Some(NavAction::Right) => {
                    self.play_ui_cue(UiSoundKind::Nav);
                    self.scroll_browser(crate::ui::browser::view::BROWSER_SCROLL_STEP)
                }
                Some(NavAction::Confirm) => {
                    self.on_browser_message(crate::ui::browser::BrowserMessage::Navigate)
                }
                Some(NavAction::Triangle) => {
                    self.on_browser_message(crate::ui::browser::BrowserMessage::ToggleUrlEntry)
                }
                Some(NavAction::Cancel) => self.browser_cancel(),
                Some(NavAction::BumperLeft) => {
                    self.on_browser_message(crate::ui::browser::BrowserMessage::Back)
                }
                Some(NavAction::BumperRight) => {
                    self.on_browser_message(crate::ui::browser::BrowserMessage::Forward)
                }
                Some(NavAction::BumperHoldLeft) => {
                    self.on_browser_message(crate::ui::browser::BrowserMessage::HistoryOpen {
                        back: true,
                    })
                }
                Some(NavAction::BumperHoldRight) => {
                    self.on_browser_message(crate::ui::browser::BrowserMessage::HistoryOpen {
                        back: false,
                    })
                }
                _ => Task::none(),
            }
        }
    }

    /// Circle/Esc at the root unwinds the browser's own modal layers before
    /// closing the window.
    pub(crate) fn browser_cancel(&mut self) -> Task<Message> {
        if self.browser.panel.bookmark_bubble {
            self.browser.panel.bookmark_bubble = false;
            return Task::none();
        }
        if self.browser.panel.url_editing {
            return self.on_browser_message(crate::ui::browser::BrowserMessage::ToggleUrlEntry);
        }
        if self.browser.panel.history_selector.is_some() {
            return self.on_browser_message(crate::ui::browser::BrowserMessage::HistoryCancel);
        }
        self.close_browser_window()
    }

    /// Keyboard while the standalone Browser window has focus.
    pub(crate) fn on_browser_key(&mut self, raw: crate::browser::keys::RawKey) -> Task<Message> {
        use crate::ui::browser::BrowserMessage;
        use iced::keyboard::Key;
        use iced::keyboard::key::Named;
        if matches!(raw.key, Key::Named(Named::Escape)) {
            if !raw.pressed {
                return Task::none();
            }
            return self.browser_cancel();
        }
        // The address field owns printable input natively while editing.
        if self.browser.panel.url_editing {
            return Task::none();
        }
        if raw.pressed {
            if raw.modifiers.control()
                && let Key::Character(text) = &raw.key
            {
                match text.to_lowercase().as_str() {
                    "l" => return self.on_browser_message(BrowserMessage::ToggleUrlEntry),
                    "r" => return self.on_browser_message(BrowserMessage::Reload),
                    _ => {}
                }
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
            Key::Named(Named::Enter) => self.on_browser_message(BrowserMessage::Navigate),
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
    let selector = page.history_selector.as_ref().map(|sel| {
        let list = if sel.back {
            session.back_entries()
        } else {
            session.forward_entries()
        };
        let rows: Vec<serde_json::Value> = list
            .iter()
            .enumerate()
            .map(|(shown, (index, entry))| {
                json!({
                    "shown": shown,
                    "index": index,
                    "url": entry.url,
                    "title": entry.title,
                })
            })
            .collect();
        json!({
            "back": sel.back,
            "highlight": sel.highlight,
            "entries": rows,
        })
    });
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
            "engine_activity": page.engine_activity.label(),
            "url_editing": panel.url_editing,
            "bookmark_bubble": panel.bookmark_bubble,
            "nav": nav_window(session.nav_entries(), session.nav_current()),
            "selector": selector,
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
    fn trace_state_exposes_nav_and_selector_entries() {
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
        panel.page.history_selector = Some(crate::ui::browser::HistorySelector {
            back: true,
            highlight: 0,
        });
        let state = trace_browser_state(&panel);
        assert_eq!(state["ui"]["nav"]["current"], 1);
        assert_eq!(state["ui"]["nav"]["entries"][0]["url"], "https://a.test/");
        assert_eq!(state["ui"]["selector"]["entries"][0]["index"], 0);
        assert_eq!(
            state["ui"]["selector"]["entries"][0]["url"],
            "https://a.test/"
        );
    }

    #[test]
    fn browser_taps_map_to_semantic_actions() {
        for (button, expected) in [
            (PadButton::Cross, NavAction::Confirm),
            (PadButton::Circle, NavAction::Cancel),
            (PadButton::Triangle, NavAction::Triangle),
            (PadButton::L3, NavAction::ToggleMagnetism),
            (PadButton::L1, NavAction::BumperLeft),
            (PadButton::R1, NavAction::BumperRight),
        ] {
            assert_eq!(
                browser_tap_action(&[tap(button)]).map(|(_, action)| action),
                Some(expected)
            );
        }
        assert_eq!(
            browser_tap_action(&[hold(PadButton::L1)]).map(|(_, action)| action),
            Some(NavAction::BumperHoldLeft)
        );
        assert_eq!(
            browser_tap_action(&[hold(PadButton::R1)]).map(|(_, action)| action),
            Some(NavAction::BumperHoldRight)
        );
        assert!(browser_tap_action(&[tap(PadButton::R3)]).is_none());
        assert!(browser_tap_action(&[hold(PadButton::Cross)]).is_none());
    }
}
