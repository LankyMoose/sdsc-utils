#![cfg_attr(windows, windows_subsystem = "windows")]

//! SDSC Utils — tray, DualSense HID, session, and iced UI in one process.

use sdsc_utils::platform::app_log;
use sdsc_utils::platform::app_meta::{
    DISPLAY_NAME, LOGIN_FLAG, PKG_NAME, PKG_VERSION, REPLACE_FLAG,
};
use sdsc_utils::platform::autostart;
use sdsc_utils::platform::crash_restart::{self, CrashArmed};
use sdsc_utils::{AppError, AppResult};
use sdsc_utils::{app, controller};
use single_instance::SingleInstance;
use std::env;
use std::io::{self, Write};
use std::process::ExitCode;
use std::sync::OnceLock;

/// Boot-owned tray-arm flag (no leak). The panic hook needs a `&'static`
/// borrow (`set_hook` contract); this `OnceLock` provides it via `get_or_init`.
static CRASH_ARMED: OnceLock<CrashArmed> = OnceLock::new();

fn crash_armed() -> &'static CrashArmed {
    CRASH_ARMED.get_or_init(CrashArmed::new)
}

fn main() -> ExitCode {
    // Crash boot owns the tray-arm flag: `OnceLock` storage outlives `main`,
    // so the panic hook (installed by `app_log::init`) can borrow it for
    // the process lifetime. CLI paths never arm it (see below); only the
    // tray path calls `arm_tray_mode`.
    let crash_armed: &'static CrashArmed = crash_armed();
    app_log::init(crash_armed);

    let args: Vec<String> = env::args().skip(1).collect();

    if args.iter().any(|a| a == crash_restart::RELAUNCH_FLAG) {
        return crash_restart::run_relauncher();
    }

    let replacing = args.iter().any(|a| a == REPLACE_FLAG);
    let test_crash_toast = args.iter().any(|a| a == "--test-crash-toast");
    let args: Vec<String> = args
        .into_iter()
        .filter(|a| a != REPLACE_FLAG && a != LOGIN_FLAG && a != "--test-crash-toast")
        .collect();
    if test_crash_toast {
        crash_restart::request_notice();
    }

    if args.iter().any(|a| a == "--help" || a == "-h") {
        attach_console_for_cli();
        print_help();
        return ExitCode::SUCCESS;
    }

    if args.iter().any(|a| a == "--version" || a == "-V") {
        attach_console_for_cli();
        println!("{PKG_NAME} {PKG_VERSION}");
        return ExitCode::SUCCESS;
    }

    if args.iter().any(|a| a == "--install-autostart") {
        attach_console_for_cli();
        return match autostart::install() {
            Ok(()) => {
                println!("Autostart installed.");
                ExitCode::SUCCESS
            }
            Err(err) => {
                eprintln!("error: {err}");
                app_log::error(format!("autostart install failed: {err}"));
                ExitCode::FAILURE
            }
        };
    }

    if args.iter().any(|a| a == "--uninstall-autostart") {
        attach_console_for_cli();
        return match autostart::uninstall() {
            Ok(()) => {
                println!("Autostart removed.");
                ExitCode::SUCCESS
            }
            Err(err) => {
                eprintln!("error: {err}");
                app_log::error(format!("autostart uninstall failed: {err}"));
                ExitCode::FAILURE
            }
        };
    }

    if args.iter().any(|a| a == "--list-controllers") {
        attach_console_for_cli();
        return list_controllers_cli();
    }

    #[cfg(debug_assertions)]
    if let Some(idx) = args.iter().position(|a| a == "--mock-releases") {
        attach_console_for_cli();
        return mock_releases_cli(&args[idx + 1..]);
    }

    if let Some(idx) = args.iter().position(|a| a == "--set-lightbar") {
        attach_console_for_cli();
        return set_lightbar_cli(&args[idx + 1..]);
    }

    if let Some(unknown) = args.first() {
        attach_console_for_cli();
        eprintln!("error: unknown argument '{unknown}'");
        eprintln!();
        print_help();
        return ExitCode::FAILURE;
    }

    // The guard must be held for the whole process lifetime. A renderer
    // reload (`--replace`) spawns its successor before exiting, so the child
    // waits briefly while the parent exits. A fresh launch instead
    // terminates the predecessor and takes over (see `prev_instance`).
    let _instance = match acquire_single_instance(replacing) {
        Ok(instance) => instance,
        Err(code) => return code,
    };

    crash_restart::arm_tray_mode(crash_armed);

    if let Err(err) = app::run() {
        app_log::error(format!("app exited with error: {err}"));
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    }
}

/// Acquire the single-instance guard.
///
/// A fresh tray launch takes over: it terminates the running predecessor,
/// then retries while the OS releases the guard. A `--replace` child is
/// spawned by its gracefully-exiting parent, so it only waits.
fn acquire_single_instance(replacing: bool) -> Result<SingleInstance, ExitCode> {
    // Fast path: no predecessor holds the guard.
    match SingleInstance::new(PKG_NAME) {
        Ok(instance) if instance.is_single() => return Ok(instance),
        Ok(_) => {}
        Err(err) => {
            app_log::error(format!("single-instance init failed: {err}"));
            return Err(ExitCode::FAILURE);
        }
    }

    if !replacing {
        sdsc_utils::platform::prev_instance::terminate_previous_instances();
    }

    // The predecessor needs a moment to die and release the guard (killed
    // takeover, or gracefully-exiting `--replace` parent).
    let attempts = if replacing { 100 } else { 50 };
    for _ in 0..attempts {
        std::thread::sleep(std::time::Duration::from_millis(100));
        match SingleInstance::new(PKG_NAME) {
            Ok(instance) => {
                if instance.is_single() {
                    return Ok(instance);
                }
            }
            Err(err) => {
                app_log::error(format!("single-instance init failed: {err}"));
                return Err(ExitCode::FAILURE);
            }
        }
    }
    if replacing {
        app_log::info("another instance is already running; exiting");
        Err(ExitCode::SUCCESS)
    } else {
        app_log::error("another instance is already running and did not exit; giving up");
        Err(ExitCode::FAILURE)
    }
}

fn print_help() {
    println!("{DISPLAY_NAME} ({PKG_NAME}) {PKG_VERSION}");
    println!();
    println!("Usage: {PKG_NAME} [OPTIONS]");
    println!();
    println!("Options:");
    println!("  -h, --help                 Show this help and exit");
    println!("  -V, --version              Print version and exit");
    println!("      --install-autostart    Windows: add a Startup entry for this exe");
    println!("      --uninstall-autostart  Windows: remove that Startup entry");
    println!("      --list-controllers     Print connected DualSense pads and exit");
    println!(
        "      --set-lightbar R G B   Set lightbar RGB (0-255) on all connected pads and exit"
    );
    println!();
    println!("With no options, starts the app (tray + HID + UI in one process).");
}

fn attach_console_for_cli() {
    #[cfg(windows)]
    {
        #[link(name = "kernel32")]
        unsafe extern "system" {
            fn AttachConsole(dw_process_id: u32) -> i32;
            fn AllocConsole() -> i32;
        }

        const ATTACH_PARENT_PROCESS: u32 = 0xFFFFFFFF;
        unsafe {
            if AttachConsole(ATTACH_PARENT_PROCESS) == 0 {
                AllocConsole();
            }
        }
        let _ = io::stdout().flush();
    }
}

fn list_controllers_cli() -> ExitCode {
    let spectrum = sdsc_utils::domain::color::SpectrumService::default();
    let claims = sdsc_utils::controller::dualsense::lightbar::ClaimStore::new();
    let lock = sdsc_utils::controller::dualsense::identity::HidIoLock::new();
    let policy = sdsc_utils::controller::hid::policy::Policy::default();
    match controller::hid::poll::poll_controllers(&lock, &policy, &[], &spectrum, &claims) {
        Ok(statuses) => {
            print_controllers(&statuses);
            ExitCode::SUCCESS
        }
        Err(err) => {
            eprintln!("error: {err}");
            ExitCode::FAILURE
        }
    }
}

fn print_controllers(statuses: &[controller::model::ControllerStatus]) {
    if statuses.is_empty() {
        println!("No DualSense controllers found.");
    } else {
        println!("{} controller(s):", statuses.len());
        for s in statuses {
            println!(
                "  #{} {} ({}) id={} {}% {}",
                s.index,
                s.product,
                s.connection,
                s.serial,
                s.percent,
                s.state.as_str()
            );
        }
    }
}

/// Debug-only: mock GitHub releases endpoint for update-check testing.
/// Release builds reject `--mock-releases` as an unknown argument (the
/// dispatch above is compiled out there too).
#[cfg(debug_assertions)]
fn mock_releases_cli(rest: &[String]) -> ExitCode {
    use sdsc_utils::platform::updater::mock;

    let port = rest
        .first()
        .and_then(|port| port.parse::<u16>().ok())
        .unwrap_or(mock::DEFAULT_PORT);
    match mock::serve(port) {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("error: mock releases failed: {err}");
            ExitCode::FAILURE
        }
    }
}

fn set_lightbar_cli(rgb_args: &[String]) -> ExitCode {
    if rgb_args.len() < 3 {
        eprintln!("error: --set-lightbar requires R G B (0-255)");
        return ExitCode::FAILURE;
    }

    let parse = |s: &str| -> AppResult<u8> {
        s.parse::<u8>()
            .map_err(|_| AppError::Config(format!("invalid RGB component '{s}' (expected 0-255)")))
    };

    let (r, g, b) = match (
        parse(&rgb_args[0]),
        parse(&rgb_args[1]),
        parse(&rgb_args[2]),
    ) {
        (Ok(r), Ok(g), Ok(b)) => (r, g, b),
        (Err(e), _, _) | (_, Err(e), _) | (_, _, Err(e)) => {
            eprintln!("error: {e}");
            return ExitCode::FAILURE;
        }
    };

    let color = sdsc_utils::domain::color::Rgb::new(r, g, b);

    let any_gamepad = hidapi::HidApi::new()
        .ok()
        .map(|api| api.device_list().any(controller::driver::is_gamepad))
        .unwrap_or(false);
    if !any_gamepad {
        eprintln!("error: no DualSense controllers found");
        return ExitCode::FAILURE;
    }
    match controller::dualsense::lightbar::apply_lightbar_all(
        &controller::dualsense::identity::HidIoLock::new(),
        color,
        &controller::dualsense::lightbar::ClaimStore::new(),
    ) {
        Ok(0) => {
            eprintln!("error: no DualSense controllers found");
            ExitCode::FAILURE
        }
        Ok(n) => {
            println!("Set lightbar to {} on {n} controller(s).", color.to_hex());
            ExitCode::SUCCESS
        }
        Err(err) => {
            eprintln!("error: {err}");
            ExitCode::FAILURE
        }
    }
}
