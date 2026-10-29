//! GitHub-release update check: fetch plus once-per-version native notify.
//!
//! Behavior lives here (next to `configure` / `popup` / `start` / `toast`);
//! pure version math and the blocking network/notification calls live in
//! [`crate::platform::updater`].

use super::{App, Message};
use crate::AppResult;
use crate::platform::app_log;
use crate::platform::app_meta::PKG_VERSION;
use crate::platform::updater::{self, ReleaseInfo, UpdateStatus};
use iced::Task;

impl App {
    /// Begin a release check unless one is already in flight.
    /// `origin` is `"boot"` or `"manual"` (log context only).
    pub(crate) fn start_update_check(&mut self, origin: &'static str) -> Task<Message> {
        if matches!(self.update_status, UpdateStatus::Checking) {
            updater::diag_update(format!(
                "update check ({origin}): already in flight, ignoring"
            ));
            return Task::none();
        }
        updater::diag_update(format!("update check ({origin}): started"));
        self.update_status = UpdateStatus::Checking;
        // `spawn_blocking` wraps the closure return in `AppResult` (cancellation
        // channel), and the fetch already returns one — flatten the nesting.
        Task::perform(
            async move {
                match super::spawn_blocking(updater::fetch_latest).await {
                    Ok(inner) => inner,
                    Err(cancelled) => Err(cancelled),
                }
            },
            Message::UpdateCheckDone,
        )
    }

    pub(crate) fn on_update_check_done(&mut self, result: AppResult<ReleaseInfo>) -> Task<Message> {
        match result {
            Err(err) => {
                app_log::warn(format!("update check failed: {err}"));
                self.update_status = UpdateStatus::Failed(short_reason(&err));
                Task::none()
            }
            Ok(info) => {
                let newer = updater::is_newer(&info.tag, PKG_VERSION);
                updater::diag_update(format!(
                    "update check: latest={} current={PKG_VERSION} newer={newer} settings_open={}",
                    info.tag,
                    u8::from(self.configure_window.is_some()),
                ));
                if !newer {
                    self.update_status = UpdateStatus::UpToDate;
                    return Task::none();
                }
                self.update_status = UpdateStatus::Available(info.clone());
                if !self.updater_state.needs_notify(&info.tag) {
                    updater::diag_update(format!(
                        "update notify: already shown for {}, skipping",
                        info.tag
                    ));
                    return Task::none();
                }
                // Record before notifying: a quit before the toast lands must
                // not re-notify on the next boot.
                self.updater_state.mark_notified(&info.tag);
                self.updater_state.save();
                updater::diag_update(format!("update notify: showing for {}", info.tag));
                let tag = info.tag.clone();
                Task::perform(
                    super::spawn_blocking(move || updater::show_update_notification(&tag)),
                    Message::UpdateNotifyDone,
                )
            }
        }
    }

    pub(crate) fn on_update_notify_done(&mut self) -> Task<Message> {
        // Fire-and-forget toast delivery; nothing follows it.
        Task::none()
    }
}

/// Single-line failure reason for the Version card (backend messages can
/// carry URLs and multi-line bodies).
fn short_reason(err: &crate::AppError) -> String {
    const MAX_REASON: usize = 160;
    let text = err.to_string();
    let line = text.lines().next().unwrap_or(&text);
    line.chars().take(MAX_REASON).collect()
}
