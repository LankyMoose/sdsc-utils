//! Delayed self-relaunch after a tray-mode panic.
//!
//! Intentional Quit does not go through the panic hook, so it never restarts.
//! A budget file under the app data dir stops crash loops.

use crate::app_log;
use crate::paths;
use serde::{Deserialize, Serialize};
use std::env;
use std::fs;
use std::process::{Command, ExitCode};
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, SystemTime};

#[cfg(windows)]
use std::os::windows::process::CommandExt;

/// Undocumented argv for the delayed relauncher process.
pub const RELAUNCH_FLAG: &str = "--crash-relaunch";

const DELAY: Duration = Duration::from_millis(1500);
const BUDGET_WINDOW_SECS: u64 = 10 * 60;
const BUDGET_MAX: usize = 3;

#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

static ARMED: AtomicBool = AtomicBool::new(false);

#[derive(Debug, Default, Serialize, Deserialize)]
struct RestartBudget {
    /// Unix epoch seconds of recent relaunch attempts.
    #[serde(default)]
    attempts: Vec<u64>,
    /// Cleared by the next tray boot via [`take_pending_notice`].
    #[serde(default)]
    notice: bool,
}

fn budget_path() -> std::path::PathBuf {
    paths::data_dir().join("crash-restart.json")
}

fn read_budget() -> RestartBudget {
    fs::read(budget_path())
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or_default()
}

/// Mark that the next tray boot should show the crash-restart toast.
pub fn request_notice() {
    let path = budget_path();
    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    let mut budget = read_budget();
    budget.notice = true;
    if let Err(err) = write_budget(&path, &budget) {
        app_log::warn(format!("crash-restart: request_notice failed: {err}"));
    }
}

/// Consume the pending crash-restart toast flag (once).
pub fn take_pending_notice() -> bool {
    let path = budget_path();
    let mut budget = read_budget();
    if !budget.notice {
        return false;
    }
    budget.notice = false;
    if let Err(err) = write_budget(&path, &budget) {
        app_log::warn(format!("crash-restart: clear notice failed: {err}"));
    }
    true
}

/// Call once tray mode is about to start (`app::run`). CLI paths stay unarmed.
pub fn arm_tray_mode() {
    ARMED.store(true, Ordering::SeqCst);
}

/// From the panic hook: spawn a detached `--crash-relaunch` helper if armed.
pub fn schedule_from_panic() {
    if !ARMED.load(Ordering::SeqCst) {
        return;
    }

    let Ok(exe) = env::current_exe() else {
        return;
    };

    let mut cmd = Command::new(&exe);
    cmd.arg(RELAUNCH_FLAG);
    #[cfg(windows)]
    cmd.creation_flags(CREATE_NO_WINDOW);

    match cmd.spawn() {
        Ok(_) => app_log::info("crash-restart: scheduled"),
        Err(err) => app_log::error(format!("crash-restart: schedule failed: {err}")),
    }
}

/// Entry for `--crash-relaunch`: wait for the dying process, then start tray mode.
pub fn run_relauncher() -> ExitCode {
    thread::sleep(DELAY);

    match consume_budget() {
        Ok(true) => {}
        Ok(false) => {
            app_log::warn(format!(
                "crash-restart: giving up after {BUDGET_MAX} restarts in {BUDGET_WINDOW_SECS}s"
            ));
            return ExitCode::FAILURE;
        }
        Err(err) => {
            app_log::warn(format!("crash-restart: budget check failed: {err}"));
            // Still try once — a bad budget file should not leave the tray dead.
        }
    }

    request_notice();

    let Ok(exe) = env::current_exe() else {
        app_log::error("crash-restart: current_exe failed");
        return ExitCode::FAILURE;
    };

    let mut cmd = Command::new(&exe);
    #[cfg(windows)]
    cmd.creation_flags(CREATE_NO_WINDOW);

    match cmd.spawn() {
        Ok(_) => {
            app_log::info("crash-restart: launching");
            ExitCode::SUCCESS
        }
        Err(err) => {
            app_log::error(format!("crash-restart: launch failed: {err}"));
            ExitCode::FAILURE
        }
    }
}

/// Returns `Ok(true)` if this attempt is within budget (and records it).
fn consume_budget() -> Result<bool, String> {
    let path = budget_path();
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }

    let now = epoch_secs();
    let cutoff = now.saturating_sub(BUDGET_WINDOW_SECS);

    let mut budget = read_budget();
    budget.attempts.retain(|&t| t >= cutoff);

    if budget.attempts.len() >= BUDGET_MAX {
        let _ = write_budget(&path, &budget);
        return Ok(false);
    }

    budget.attempts.push(now);
    write_budget(&path, &budget)?;
    Ok(true)
}

fn write_budget(path: &std::path::Path, budget: &RestartBudget) -> Result<(), String> {
    let bytes = serde_json::to_vec_pretty(budget).map_err(|e| e.to_string())?;
    fs::write(path, bytes).map_err(|e| e.to_string())
}

fn epoch_secs() -> u64 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}
