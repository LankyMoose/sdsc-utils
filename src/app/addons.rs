//! Browser add-on package lifecycle: install, update, uninstall, launch.
//!
//! Home treats the add-on as the managed app and the CEF engine as its
//! runtime dependency. Install/update copies the add-on first, then chains the
//! engine package when it is missing or has an update. Every transition emits
//! a `home` action/phase flow with replay data.

use super::browser::EngineStatus;
use super::{App, Message};
use crate::browser::addon_install::{self, AddonInstallOutcome, AddonProgress, AddonStatus};
use crate::platform::trace::{self, Lane};
use crate::ui::home::{EnginePhase, EngineProgress};
use iced::Task;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

impl App {
    pub(crate) fn refresh_addon_status(&mut self) -> AddonStatus {
        let status = addon_install::status();
        self.browser.addon_installed = status.installed.is_some() || status.managed;
        self.browser.addon_managed = status.managed;
        self.browser.addon_version = status
            .installed
            .as_ref()
            .map(|meta| meta.version.clone())
            .filter(|version| !version.is_empty())
            .or_else(|| {
                status
                    .managed
                    .then(|| crate::platform::app_meta::PKG_VERSION.to_string())
            });
        self.browser.addon_latest_version =
            status.source.as_ref().map(|source| source.version.clone());
        // A package-owned add-on is updated with the app, never by Home.
        self.browser.addon_update_ready = status.update_ready && !status.managed;
        self.browser.addon_source_available = status.source.is_some();
        self.browser.addon_size = status.installed.as_ref().map(|meta| meta.size);
        self.browser.engine_available = addon_install::runtime_path().is_some();
        status
    }

    pub(crate) fn sync_home_app_state(&mut self) {
        self.home_state.app_managed = self.browser.addon_managed;
        self.home_state.set_app_state(
            self.browser.addon_installed,
            self.browser.addon_installed && self.browser.engine_status.is_ready(),
            self.browser.addon_version.clone(),
            self.browser.addon_update_ready || self.home_state.engine_update_ready,
        );
    }

    /// Install/update the Browser app. `update` is advisory: a missing add-on
    /// is always a fresh install. The engine component is chained when it is
    /// missing or has an update.
    pub(crate) fn start_browser_app_install(
        &mut self,
        update: bool,
        source_override: Option<PathBuf>,
        version_override: Option<String>,
    ) -> Task<Message> {
        let status = self.refresh_addon_status();
        self.sync_home_app_state();
        let installed = status.installed.is_some();
        let update = update && installed;
        let action_name = if update {
            "home.app.update"
        } else {
            "home.app.install"
        };
        let source_label = if crate::platform::trace::replay::replaying() {
            "replay"
        } else {
            "home"
        };
        let source = source_override
            .as_deref()
            .and_then(|path| addon_install::source_at(path, version_override.clone()))
            .or(status.source.clone());
        let repro_params = source
            .as_ref()
            .map(|source| {
                serde_json::json!({
                    "source": source.path.display().to_string(),
                    "version": source.version,
                })
            })
            .unwrap_or_else(|| serde_json::json!({}));
        let mut builder = trace::action(Lane::Home, action_name)
            .app("browser")
            .source(source_label)
            .field("update", update)
            .field("installed", installed)
            .pre(self.trace_home_state())
            .repro(serde_json::json!({
                "app": "home",
                "action": if update { "app.update" } else { "app.install" },
                "params": repro_params,
            }));
        if let Some(source) = &source {
            builder = builder
                .field("source", source.path.display().to_string())
                .field("version", &source.version);
        }
        builder = builder.require("addon.installed", installed);
        if update
            && let Some(version) = status
                .installed
                .as_ref()
                .map(|meta| meta.version.clone())
                .filter(|version| !version.is_empty())
        {
            builder = builder.require("addon.version", version);
        }
        let action = builder.begin();

        if update && !installed {
            action
                .finish("failed")
                .field("reason", "not_installed")
                .post(self.trace_home_state())
                .emit();
            return Task::none();
        }
        if status.managed {
            // Store-owned add-on: updates arrive with the app package.
            action
                .finish("managed")
                .field("reason", "package_owned")
                .post(self.trace_home_state())
                .emit();
            return Task::none();
        }

        // The add-on alone is not enough to launch; chain the engine component
        // whenever it is missing or has a pending update.
        let chain_engine =
            !self.browser.engine_status.is_ready() || self.home_state.engine_update_ready;
        let addon_update_ready = match (&status.installed, &source) {
            (Some(meta), Some(source)) => {
                !meta.protocol_compatible()
                    || meta.version != source.version
                    || meta.size != source.size
                    || meta.source_modified != source.modified
            }
            _ => status.update_ready,
        };
        if installed && !(update && addon_update_ready) {
            if chain_engine {
                return self.start_engine_component(action, update);
            }
            action
                .finish("up_to_date")
                .post(self.trace_home_state())
                .emit();
            return Task::none();
        }

        let Some(source) = source else {
            action
                .finish("failed")
                .field("reason", "source_missing")
                .post(self.trace_home_state())
                .emit();
            self.home_state.install_done(
                false,
                Some("Browser add-on package is not available".to_string()),
            );
            return Task::none();
        };

        let close = if self.browser.remote.is_some() {
            self.close_remote_browser()
        } else {
            Task::none()
        };
        self.browser.addon_pending_action = Some(action.detach());
        self.browser.addon_pending_update = update;
        self.browser.addon_chain_engine = chain_engine;
        self.browser.addon_copying = true;
        self.home_state.set_installing(true);
        self.home_state.set_install_progress(EngineProgress {
            phase: EnginePhase::Installing,
            fraction: Some(0.0),
            bytes_done: 0,
            bytes_total: Some(source.size),
            rate_bps: None,
        });
        self.browser.addon_install_generation =
            self.browser.addon_install_generation.wrapping_add(1);
        let generation = self.browser.addon_install_generation;
        let cancel = Arc::new(AtomicBool::new(false));
        self.browser.addon_install_cancel = Arc::clone(&cancel);
        let worker_cancel = Arc::clone(&cancel);
        let (progress_tx, progress_rx) = iced::futures::channel::mpsc::unbounded::<AddonProgress>();
        let progress_task =
            Task::stream(progress_rx).map(move |progress| Message::AddonInstallProgress {
                generation,
                progress,
            });
        let install = Task::perform(
            async move {
                super::spawn_blocking(move || {
                    let mut progress = |progress| {
                        let _ = progress_tx.unbounded_send(progress);
                    };
                    addon_install::install(&source, &mut progress, &worker_cancel).map_err(|err| {
                        match err {
                            addon_install::AddonError::Cancelled => AddonInstallOutcome::Cancelled,
                            addon_install::AddonError::Failed(message) => {
                                AddonInstallOutcome::Failed(message)
                            }
                        }
                    })
                })
                .await
            },
            move |result| Message::AddonInstallDone {
                generation,
                outcome: match result {
                    Ok(Ok(meta)) => AddonInstallOutcome::Installed(meta),
                    Ok(Err(outcome)) => outcome,
                    Err(err) => AddonInstallOutcome::Failed(err.to_string()),
                },
            },
        );
        close.chain(Task::batch([progress_task, install]))
    }

    fn start_engine_component(
        &mut self,
        action: crate::platform::trace::ActionGuard,
        update: bool,
    ) -> Task<Message> {
        self.browser.addon_pending_action = Some(action.detach());
        self.browser.addon_pending_update = update;
        self.browser.addon_chain_engine = true;
        self.home_state.set_installing(true);
        let task = self.start_engine_install();
        if !matches!(self.browser.engine_status, EngineStatus::Installing) {
            // Packaged builds hand off to the Store synchronously; there is no
            // async completion to settle this flow.
            self.finish_pending_app("failed", Some("Browser engine install unavailable"));
        }
        task
    }

    pub(crate) fn cancel_browser_app_install(&mut self) -> Task<Message> {
        if self.browser.addon_copying {
            self.browser
                .addon_install_cancel
                .store(true, Ordering::Relaxed);
            self.home_state.set_cancelling();
            let mut event = trace::event(Lane::Home, "browser.addon.install.cancel")
                .field("component", "addon");
            if let Some(flow) = self.pending_app_flow() {
                event = event.flow(flow);
            }
            event.emit();
        }
        let task = self.cancel_engine_install();
        if matches!(self.browser.engine_status, EngineStatus::Installing) {
            self.home_state.set_cancelling();
        }
        task
    }

    pub(crate) fn start_browser_app_uninstall(&mut self) -> Task<Message> {
        let status = self.refresh_addon_status();
        self.sync_home_app_state();
        let engine_ready = self.browser.engine_status.is_ready();
        if status.installed.is_none() && !engine_ready {
            trace::event(Lane::Home, "browser.addon.uninstall")
                .field("reason", "nothing_installed")
                .emit();
            return Task::none();
        }
        let source_label = if crate::platform::trace::replay::replaying() {
            "replay"
        } else {
            "home"
        };
        let action = trace::action(Lane::Home, "home.app.uninstall")
            .app("browser")
            .source(source_label)
            .field("addon_installed", status.installed.is_some())
            .field("engine_ready", engine_ready)
            .pre(self.trace_home_state())
            .repro(serde_json::json!({
                "app": "home",
                "action": "app.uninstall",
                "params": {},
            }))
            .require("addon.installed", status.installed.is_some())
            .begin();
        let close = if self.browser.remote.is_some() {
            self.close_remote_browser()
        } else {
            Task::none()
        };
        self.browser.addon_pending_action = Some(action.detach());
        self.browser.addon_pending_update = false;
        self.browser.addon_chain_engine = engine_ready;
        self.browser.addon_install_generation =
            self.browser.addon_install_generation.wrapping_add(1);
        let generation = self.browser.addon_install_generation;
        self.home_state.set_uninstalling();
        trace::event(Lane::Home, "browser.addon.uninstall.start")
            .field("engine_ready", engine_ready)
            .emit();
        let uninstall = Task::perform(
            async move {
                super::spawn_blocking(addon_install::uninstall)
                    .await
                    .map_err(|err| err.to_string())
                    .and_then(|result| result.map_err(|err| err.to_string()))
            },
            move |result| Message::AddonUninstallDone { generation, result },
        );
        close.chain(uninstall)
    }

    pub(crate) fn on_addon_install_progress(
        &mut self,
        generation: u64,
        progress: AddonProgress,
    ) -> Task<Message> {
        if generation != self.browser.addon_install_generation
            || !self.browser.addon_copying
            || self.home_state.cancelling
        {
            return Task::none();
        }
        let mapped = match progress {
            AddonProgress::Copying { done, total } => EngineProgress {
                phase: EnginePhase::Installing,
                fraction: (total > 0).then(|| done as f32 / total as f32),
                bytes_done: done,
                bytes_total: Some(total),
                rate_bps: None,
            },
            AddonProgress::Finishing => EngineProgress {
                phase: EnginePhase::Finishing,
                fraction: Some(1.0),
                bytes_done: 0,
                bytes_total: None,
                rate_bps: None,
            },
        };
        self.home_state.set_install_progress(mapped);
        if let Some(pending) = &self.browser.addon_pending_action {
            trace::event(Lane::Home, "browser.addon.install.phase")
                .flow(pending.flow())
                .field("phase", mapped.label())
                .field("bytes_done", mapped.bytes_done)
                .field("bytes_total", mapped.bytes_total)
                .emit();
        }
        Task::none()
    }

    pub(crate) fn on_addon_install_done(
        &mut self,
        generation: u64,
        outcome: AddonInstallOutcome,
    ) -> Task<Message> {
        if generation != self.browser.addon_install_generation {
            return Task::none();
        }
        self.browser.addon_copying = false;
        self.refresh_addon_status();
        match outcome {
            AddonInstallOutcome::Installed(meta) => {
                trace::event(Lane::Home, "browser.addon.install.done")
                    .field("version", &meta.version)
                    .field("size", meta.size)
                    .emit();
                trace::manifest(
                    "browser_addon",
                    serde_json::json!({
                        "state": "installed",
                        "version": meta.version,
                        "protocol": meta.protocol,
                        "size": meta.size,
                        "source": meta.source,
                    }),
                );
                if self.browser.addon_chain_engine
                    || !self.browser.engine_status.is_ready()
                    || self.home_state.engine_update_ready
                {
                    let task = self.start_engine_install();
                    if matches!(self.browser.engine_status, EngineStatus::Installing) {
                        return task;
                    }
                    self.finish_pending_app("failed", Some("Browser engine install unavailable"));
                    return task;
                }
                self.home_state.install_done(true, None);
                self.sync_home_app_state();
                let outcome = if self.browser.addon_pending_update {
                    "updated"
                } else {
                    "installed"
                };
                self.finish_pending_app(outcome, None);
            }
            AddonInstallOutcome::Cancelled => {
                self.home_state
                    .install_cancelled(self.browser.engine_status.is_ready());
                self.sync_home_app_state();
                self.finish_pending_app("cancelled", None);
            }
            AddonInstallOutcome::Failed(message) => {
                self.home_state.install_done(false, Some(message.clone()));
                self.sync_home_app_state();
                self.finish_pending_app("failed", Some(&message));
            }
        }
        Task::none()
    }

    pub(crate) fn on_addon_uninstall_done(
        &mut self,
        generation: u64,
        result: Result<(), String>,
    ) -> Task<Message> {
        if generation != self.browser.addon_install_generation {
            return Task::none();
        }
        self.refresh_addon_status();
        match result {
            Ok(()) => {
                trace::event(Lane::Home, "browser.addon.uninstall.done").emit();
                trace::manifest(
                    "browser_addon",
                    serde_json::json!({ "state": "uninstalled" }),
                );
                if self.browser.addon_chain_engine
                    && self.browser.engine_status.is_ready()
                    && self.engine_locally_uninstallable()
                {
                    let task = self.start_engine_uninstall();
                    if matches!(self.browser.engine_status, EngineStatus::Uninstalling) {
                        return task;
                    }
                    self.home_state.uninstall_done(None);
                    self.sync_home_app_state();
                    self.finish_pending_app("uninstalled", None);
                    return task;
                }
                self.home_state.uninstall_done(None);
                self.sync_home_app_state();
                self.finish_pending_app("uninstalled", None);
            }
            Err(message) => {
                self.home_state.uninstall_done(Some(message.clone()));
                self.sync_home_app_state();
                self.finish_pending_app("failed", Some(&message));
            }
        }
        Task::none()
    }

    /// Close the app action flow after any chained engine work.
    pub(crate) fn finish_pending_app(&mut self, outcome: &'static str, error: Option<&str>) {
        self.refresh_addon_status();
        self.sync_home_app_state();
        let Some(pending) = self.browser.addon_pending_action.take() else {
            return;
        };
        let state = self.trace_home_state();
        let mut result = pending
            .settle(outcome)
            .field("addon_installed", self.browser.addon_installed)
            .field("addon_version", self.browser.addon_version.clone())
            .field("engine_ready", self.browser.engine_status.is_ready())
            .check(
                "addon_state",
                if outcome == "uninstalled" {
                    !self.browser.addon_installed
                } else {
                    self.browser.addon_installed
                },
            )
            .post(state);
        if let Some(error) = error {
            result = result.field("error", error);
        }
        result.emit();
        self.browser.addon_chain_engine = false;
        self.browser.addon_pending_update = false;
    }

    #[cfg(feature = "cef-engine")]
    fn engine_locally_uninstallable(&self) -> bool {
        let EngineStatus::Ready(path) = &self.browser.engine_status else {
            return false;
        };
        crate::browser::engine_install::active_local_dir(&crate::persist::paths::data_dir())
            .as_deref()
            == Some(path.as_path())
    }

    #[cfg(not(feature = "cef-engine"))]
    fn engine_locally_uninstallable(&self) -> bool {
        false
    }

    pub(crate) fn pending_app_flow(&self) -> Option<crate::platform::trace::FlowId> {
        self.browser
            .addon_pending_action
            .as_ref()
            .map(|pending| pending.flow())
    }

    /// Settle the app action after a chained engine install finishes.
    #[cfg(feature = "cef-engine")]
    pub(crate) fn finish_app_after_engine_install(
        &mut self,
        success: bool,
        cancelled: bool,
        error: Option<&str>,
    ) {
        if cancelled {
            self.finish_pending_app("cancelled", None);
        } else if success {
            let outcome = if self.browser.addon_pending_update {
                "updated"
            } else {
                "installed"
            };
            self.finish_pending_app(outcome, error);
        } else {
            self.finish_pending_app("failed", error);
        }
    }

    /// Settle the app action after a chained engine uninstall finishes.
    #[cfg(feature = "cef-engine")]
    pub(crate) fn finish_app_after_engine_uninstall(&mut self, result: &Result<(), String>) {
        match result {
            Ok(()) => self.finish_pending_app("uninstalled", None),
            Err(err) => self.finish_pending_app("failed", Some(err)),
        }
    }
}
