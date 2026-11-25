#![cfg_attr(windows, windows_subsystem = "windows")]

//! Optional Browser add-on process. Attaches to the host over stdin/stdout
//! framing and owns its own iced window + CEF engine.

use std::process::ExitCode;

fn main() -> ExitCode {
    // CEF re-executes this binary for renderer/GPU/utility processes.
    #[cfg(feature = "cef-engine")]
    if let Some(code) = sdsc_utils::browser::cef_backend::execute_subprocess() {
        return ExitCode::from(u8::try_from(code).unwrap_or(1));
    }

    #[cfg(feature = "cef-engine")]
    {
        match sdsc_utils::browser::server::run() {
            Ok(()) => ExitCode::SUCCESS,
            Err(err) => {
                eprintln!("sdsc-browser: {err}");
                ExitCode::FAILURE
            }
        }
    }
    #[cfg(not(feature = "cef-engine"))]
    {
        eprintln!("sdsc-browser: built without the CEF engine feature");
        ExitCode::FAILURE
    }
}
