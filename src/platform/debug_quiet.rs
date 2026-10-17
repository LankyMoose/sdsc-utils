//! Debug-only "quiet" UI mode for screenshotting while the desktop is in use.
//!
//! Enabled when `SDSC_DEBUG_OPEN` is set (debug builds only). While quiet:
//! windows never take focus and are relocated from the primary monitor to a
//! secondary one (`ui::layout::{focus_window, show_window, move_window}`), and
//! UI sounds / Start haptics are muted — so a debug session cannot interrupt a
//! game running on the primary monitor. Release builds always return `false`.

/// Env var that opens windows at boot and turns quiet mode on.
pub const DEBUG_OPEN_ENV: &str = "SDSC_DEBUG_OPEN";

/// True while debug quiet mode is active (never in release builds).
pub fn enabled() -> bool {
    #[cfg(debug_assertions)]
    {
        use std::sync::OnceLock;
        static QUIET: OnceLock<bool> = OnceLock::new();
        *QUIET.get_or_init(|| std::env::var_os(DEBUG_OPEN_ENV).is_some())
    }
    #[cfg(not(debug_assertions))]
    {
        false
    }
}
