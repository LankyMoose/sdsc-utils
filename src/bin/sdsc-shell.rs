#![cfg_attr(windows, windows_subsystem = "windows")]

//! SDSC Utils iced shell — windows only; attaches to the HID/tray service over IPC.
//!
//! When launched without the service (`SDSC_UTILS_PIPE` unset), runs the legacy
//! in-process stack (HID + tray + UI) for local development.

use sdsc_utils::app;
use sdsc_utils::ipc::SHELL_PIPE_ENV;
use sdsc_utils::platform::app_log;
use sdsc_utils::platform::crash_restart;
use std::env;
use std::process::ExitCode;

fn main() -> ExitCode {
    app_log::init();

    let args: Vec<String> = env::args().skip(1).collect();
    if args.iter().any(|a| a == crash_restart::RELAUNCH_FLAG) {
        return crash_restart::run_relauncher();
    }

    let as_client = env::var_os(SHELL_PIPE_ENV).is_some();
    if as_client {
        // Service owns crash-restart / respawn. A shell panic must not relaunch
        // a second shell beside the service's respawn (and must not touch HID).
        match app::run_shell_client() {
            Ok(()) => ExitCode::SUCCESS,
            Err(err) => {
                app_log::error(format!("shell client exited: {err}"));
                ExitCode::FAILURE
            }
        }
    } else {
        // Standalone / developer path: full in-process daemon. Developer
        // tooling (emulated controllers) is compiled into debug builds, so
        // this needs no flag.
        crash_restart::arm_tray_mode();
        match app::run() {
            Ok(()) => ExitCode::SUCCESS,
            Err(err) => {
                app_log::error(format!("shell standalone exited: {err}"));
                ExitCode::FAILURE
            }
        }
    }
}
