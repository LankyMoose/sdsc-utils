#![cfg_attr(windows, windows_subsystem = "windows")]

//! SDSC Utils — tray, DualSense HID, session, and iced UI in one process.

use sdsc_utils::platform::app_log;
use sdsc_utils::platform::app_meta::{DISPLAY_NAME, PKG_NAME, PKG_VERSION, REPLACE_FLAG};
use sdsc_utils::platform::autostart;
use sdsc_utils::platform::crash_restart;
use sdsc_utils::{app, controller};
use single_instance::SingleInstance;
use std::env;
use std::io::{self, Write};
use std::process::ExitCode;

fn main() -> ExitCode {
    app_log::init();

    let args: Vec<String> = env::args().skip(1).collect();

    if args.iter().any(|a| a == crash_restart::RELAUNCH_FLAG) {
        return crash_restart::run_relauncher();
    }

    let replacing = args.iter().any(|a| a == REPLACE_FLAG);
    let test_crash_toast = args.iter().any(|a| a == "--test-crash-toast");
    let args: Vec<String> = args
        .into_iter()
        .filter(|a| a != REPLACE_FLAG && a != "--test-crash-toast")
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
    // retries briefly while the parent releases the guard; an ordinary
    // second launch exits at once.
    let _instance = match acquire_single_instance(replacing) {
        Ok(instance) => instance,
        Err(code) => return code,
    };

    crash_restart::arm_tray_mode();

    if let Err(err) = app::run() {
        app_log::error(format!("app exited with error: {err}"));
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    }
}

/// Acquire the single-instance guard, retrying briefly for `--replace`.
fn acquire_single_instance(replacing: bool) -> Result<SingleInstance, ExitCode> {
    let attempts = if replacing { 100 } else { 1 };
    for _ in 0..attempts {
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
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    app_log::info("another instance is already running; exiting");
    Err(ExitCode::SUCCESS)
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
    match controller::hid::poll::poll_controllers(&[]) {
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

fn set_lightbar_cli(rgb_args: &[String]) -> ExitCode {
    if rgb_args.len() < 3 {
        eprintln!("error: --set-lightbar requires R G B (0-255)");
        return ExitCode::FAILURE;
    }

    let parse = |s: &str| -> Result<u8, String> {
        s.parse::<u8>()
            .map_err(|_| format!("invalid RGB component '{s}' (expected 0-255)"))
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
    match controller::dualsense::lightbar::apply_lightbar_all(color) {
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
