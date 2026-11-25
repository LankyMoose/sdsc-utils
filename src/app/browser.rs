//! Top-level Browser add-on state.
//!
//! The shell owns launch/lifecycle, Home, the system OSK, and the engine
//! package install. The `sdsc-browser` child owns its window, CEF, cursor,
//! page state, and text session.

use crate::domain::pad::PadAction;
#[cfg(debug_assertions)]
use crate::domain::pad::PadButton;
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

#[derive(Default)]
pub struct BrowserAppState {
    /// True when the add-on executable is available.
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
    /// Optional Browser add-on process. `None` means the Browser is closed or
    /// the add-on is not installed.
    pub remote: Option<super::browser_remote::BrowserRemote>,
    /// Latest published add-on app spec.
    pub remote_spec: Option<crate::hosted::AppSpec>,
    /// Last add-on launch/protocol error, for Home/diagnostics.
    pub remote_error: Option<String>,
    /// Latest state snapshot from the add-on's replay handler.
    pub replay_state: Option<serde_json::Value>,
}

use iced::Task;
#[cfg(feature = "cef-engine")]
use std::time::Instant;

use super::Message;

const REPLAY_BROWSER_WAIT: std::time::Duration = std::time::Duration::from_millis(2500);
const REPLAY_STEP_WAIT: std::time::Duration = std::time::Duration::from_millis(1500);

#[cfg(debug_assertions)]
fn browser_replay_button(name: &str) -> Option<PadButton> {
    Some(match name {
        "cross" => PadButton::Cross,
        "circle" => PadButton::Circle,
        "square" => PadButton::Square,
        "triangle" => PadButton::Triangle,
        "l3" => PadButton::L3,
        "r3" => PadButton::R3,
        "l1" => PadButton::L1,
        "r1" => PadButton::R1,
        "options" => PadButton::Options,
        _ => return None,
    })
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
        let Some(step) = self.replay.as_mut().and_then(|plan| plan.next_step()) else {
            self.replay = None;
            crate::platform::trace::manifest("replay.done", serde_json::json!({ "flow": flow }));
            // Automated sessions must not leave the browser on screen after
            // the plan finishes. The trace is fully written before close.
            return self.close_browser_window();
        };
        if step.app == "browser" {
            let state = self.browser_trace_state();
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

    pub(crate) fn browser_trace_state(&self) -> serde_json::Value {
        self.browser
            .replay_state
            .clone()
            .unwrap_or(serde_json::Value::Null)
    }

    #[cfg(debug_assertions)]
    fn remote_replay_step(
        &mut self,
        step: &crate::platform::trace::replay::ReplayStep,
    ) -> Task<Message> {
        match step.action.as_str() {
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
            "osk.open" => self.request_osk_open(crate::ui::osk::OskTarget::BrowserPage),
            "osk.close" => self.close_osk("replay"),
            "key" => {
                let Some(key) = step.params.get("key").and_then(serde_json::Value::as_str) else {
                    return Task::none();
                };
                let close = self.close_osk("keyboard");
                if matches!(key.to_ascii_lowercase().as_str(), "escape" | "esc") {
                    return close;
                }
                if let Some(remote) = &mut self.browser.remote {
                    remote.send_replay("key", step.params.clone());
                }
                close
            }
            "ime.preedit" | "ime.commit" => {
                let close = self.close_osk("ime");
                if let Some(remote) = &mut self.browser.remote {
                    remote.send_replay(&step.action, step.params.clone());
                }
                close
            }
            "mouse.click" | "mouse.wheel" if self.osk.state.open => self.close_osk("mouse"),
            "pad.tap" => {
                let button = step
                    .params
                    .get("button")
                    .and_then(serde_json::Value::as_str)
                    .and_then(browser_replay_button);
                if let Some(button) = button
                    && self.osk.state.open
                {
                    return self.replay_pad_tap(button);
                }
                if button.is_some() {
                    crate::platform::trace::event(
                        crate::platform::trace::Lane::Platform,
                        "replay.pad_tap",
                    )
                    .field(
                        "button",
                        step.params
                            .get("button")
                            .and_then(serde_json::Value::as_str)
                            .unwrap_or_default(),
                    )
                    .emit();
                }
                if let Some(button) = button
                    && let Some(action) = crate::app::actions::browser_action(button)
                {
                    let enabled = self
                        .browser
                        .remote_spec
                        .as_ref()
                        .is_some_and(|spec| spec.action_enabled(action));
                    crate::platform::trace::event(
                        crate::platform::trace::Lane::Platform,
                        "app.action",
                    )
                    .field("app", crate::app::actions::BROWSER_APP)
                    .field("action", action)
                    .field("known_app", true)
                    .field("enabled", enabled)
                    .emit();
                }
                if let Some(remote) = &mut self.browser.remote {
                    remote.send_replay(&step.action, step.params.clone());
                }
                Task::none()
            }
            "screenshot" | "text" | "navigate" | "click" | "cursor.dock" | "pad.dir"
            | "history.back" | "history.forward" | "mouse.click" | "mouse.drag" | "mouse.wheel" => {
                if let Some(remote) = &mut self.browser.remote {
                    remote.send_replay(&step.action, step.params.clone());
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
        if self.browser.remote.is_none() {
            crate::platform::trace::event(
                crate::platform::trace::Lane::Platform,
                "replay.unsupported",
            )
            .field("app", &step.app)
            .field("action", &step.action)
            .field("reason", "browser_addon_missing")
            .emit();
            return Task::none();
        }
        #[cfg(debug_assertions)]
        {
            self.remote_replay_step(step)
        }
        #[cfg(not(debug_assertions))]
        {
            Task::none()
        }
    }
}

impl super::App {
    /// Open the Browser add-on. There is no in-process Browser fallback: the
    /// child process owns its window, CEF, cursor, and text session.
    pub(crate) fn open_browser_window(&mut self) -> Task<Message> {
        if self.browser.remote.is_some() {
            return Task::none();
        }
        if !crate::app::browser_remote::remote_enabled() {
            let message = "Browser add-on is not installed".to_string();
            crate::platform::app_log::warn(message.clone());
            self.browser.remote_error = Some(message);
            return Task::none();
        }
        self.open_remote_browser()
    }

    pub(crate) fn close_browser_window(&mut self) -> Task<Message> {
        self.close_remote_browser()
    }

    /// Browser text-session client: insert text from the system OSK.
    pub(crate) fn browser_text_insert(&mut self, text: &str) {
        if self.browser.remote.is_some() {
            self.remote_browser_text(crate::hosted::TextAction::Insert(text.to_string()));
        }
    }

    /// Browser text-session client: backspace in the focused page field.
    pub(crate) fn browser_text_backspace(&mut self) {
        if self.browser.remote.is_some() {
            self.remote_browser_text(crate::hosted::TextAction::Backspace);
        }
    }

    /// Browser text-session client: Enter in the focused page field.
    pub(crate) fn browser_text_enter(&mut self) {
        if self.browser.remote.is_some() {
            self.remote_browser_text(crate::hosted::TextAction::Enter);
        }
    }

    /// Pad tick while the Browser add-on owns the surface key set. Home/PS is
    /// handled here; everything else is streamed to the child.
    pub(crate) fn on_browser_pad(
        &mut self,
        readings: &[crate::ui::launcher::input::NavReading],
        tap_actions: &[PadAction],
    ) -> Task<Message> {
        if readings.is_empty() || self.browser.remote.is_none() {
            return Task::none();
        }
        if self.home_chord_fires(readings) {
            crate::controller::hid::diag::diag_info("ui-diag: PS opens home over Browser");
            return self.toggle_home();
        }
        self.remote_browser_pad(readings, tap_actions)
    }
}

impl super::App {
    /// Re-check the add-on executable and the optional engine package.
    /// The shell never spawns CEF; the add-on process owns the engine. The
    /// install/uninstall UI stays keyed to the engine directory, while the
    /// launch gate additionally requires the add-on executable.
    #[cfg(feature = "cef-engine")]
    pub(crate) fn refresh_engine_status(&mut self) {
        self.browser.engine_available = crate::app::browser_remote::addon_available();
        self.browser.engine_managed = false;
        self.browser.engine_status = match crate::browser::cef_backend::resolve_engine_dir() {
            Some(path) => {
                self.browser.engine_managed = crate::browser::cef_backend::engine_is_managed(&path);
                EngineStatus::Ready(path)
            }
            None => EngineStatus::Missing,
        };
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
        self.browser.engine_available = false;
        let uninstall = Task::perform(
            async move {
                super::spawn_blocking(move || {
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
                self.browser.engine_status = EngineStatus::Ready(path);
                self.browser.engine_available = true;
                self.browser.engine_managed = false;
                // A running add-on keeps its loaded CEF until it is relaunched.
                self.browser.engine_restart_required = self.browser.remote.is_some();
                self.home_state.install_done(true, None);
                crate::platform::app_log::info(format!(
                    "cef: BrowserEngine install complete{}",
                    if self.browser.engine_restart_required {
                        " (restart required)"
                    } else {
                        ""
                    }
                ));
            }
            EngineInstallOutcome::Cancelled => {
                let previous = self.browser.engine_install_previous.take();
                let ready = previous.as_ref().is_some_and(EngineStatus::is_ready);
                self.browser.engine_status = previous.unwrap_or(EngineStatus::Missing);
                self.browser.engine_available = crate::app::browser_remote::addon_available();
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
#[cfg_attr(not(feature = "cef-engine"), allow(dead_code))]
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
        assert!(state["ui"].get("url_editing").is_none());
    }
}
