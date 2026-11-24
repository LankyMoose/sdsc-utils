//! Debug-only "quiet" UI mode for screenshotting while the desktop is in use.
//!
//! Enabled when `SDSC_DEBUG_OPEN` is set (debug builds only), unless
//! `SDSC_DEBUG_QUIET=0` explicitly opts out. `SDSC_DEBUG_QUIET=1` also enables
//! quiet mode on its own, so an automated launch that auto-opens a window
//! without `SDSC_DEBUG_OPEN` still goes to the secondary monitor. While quiet:
//! windows never take focus and are relocated from the primary monitor to a
//! secondary one (`ui::layout::{focus_window, show_window, move_window}`), real
//! keyboard/mouse/touch input is locked out, pad edges are dropped, and UI
//! sounds / Launcher haptics are muted — so a debug session cannot interrupt a
//! game running on the primary monitor. Set `SDSC_DEBUG_INPUT=1` to keep real
//! input live in quiet mode (manual QA). Release builds always return `false`.

/// Env var that opens windows at boot; turns quiet mode on by default.
pub const DEBUG_OPEN_ENV: &str = "SDSC_DEBUG_OPEN";

/// Opt out of quiet mode while keeping [`DEBUG_OPEN_ENV`] auto-open.
/// Set to `0`, `false`, or `off`.
pub const DEBUG_QUIET_ENV: &str = "SDSC_DEBUG_QUIET";

/// Keep real keyboard/mouse/touch/pad input live while quiet mode still owns
/// window placement/focus. Set to `1`, `true`, or `on`.
pub const DEBUG_INPUT_ENV: &str = "SDSC_DEBUG_INPUT";

/// True while debug quiet mode is active (never in release builds).
pub fn enabled() -> bool {
    #[cfg(debug_assertions)]
    {
        use std::sync::OnceLock;
        static QUIET: OnceLock<bool> = OnceLock::new();
        *QUIET.get_or_init(|| {
            let open = std::env::var_os(DEBUG_OPEN_ENV).is_some();
            let quiet = std::env::var(DEBUG_QUIET_ENV).ok();
            quiet_from_env(open, quiet.as_deref())
        })
    }
    #[cfg(not(debug_assertions))]
    {
        false
    }
}

/// True when quiet mode should swallow real keyboard/mouse/touch/pad input.
pub fn input_dropped() -> bool {
    #[cfg(debug_assertions)]
    {
        use std::sync::OnceLock;
        static DROPPED: OnceLock<bool> = OnceLock::new();
        *DROPPED.get_or_init(|| {
            let input = std::env::var(DEBUG_INPUT_ENV).ok();
            input_dropped_from_env(enabled(), input.as_deref())
        })
    }
    #[cfg(not(debug_assertions))]
    {
        false
    }
}

#[cfg(any(debug_assertions, test))]
fn quiet_from_env(open: bool, quiet: Option<&str>) -> bool {
    match quiet.map(str::to_ascii_lowercase).as_deref() {
        Some("1" | "true" | "on") => true,
        Some("0" | "false" | "off") => false,
        _ => open,
    }
}

#[cfg(any(debug_assertions, test))]
fn input_dropped_from_env(quiet: bool, input: Option<&str>) -> bool {
    match input.map(str::to_ascii_lowercase).as_deref() {
        // Explicit opt-in/opt-out, even outside quiet mode. This lets the QA
        // console run a focused primary-monitor session with pad edges off.
        Some("1" | "true" | "on") => false,
        Some("0" | "false" | "off") => true,
        // Unset follows the mode: quiet sessions drop edges, normal sessions
        // keep them live.
        _ => quiet,
    }
}

#[cfg(test)]
mod tests {
    use super::{input_dropped_from_env, quiet_from_env};

    #[test]
    fn auto_open_defaults_to_quiet() {
        assert!(quiet_from_env(true, None));
    }

    #[test]
    fn auto_open_can_opt_out() {
        assert!(!quiet_from_env(true, Some("0")));
        assert!(!quiet_from_env(true, Some("false")));
        assert!(!quiet_from_env(true, Some("OFF")));
    }

    #[test]
    fn explicit_quiet_works_without_auto_open() {
        assert!(!quiet_from_env(false, None));
        assert!(quiet_from_env(false, Some("1")));
        assert!(quiet_from_env(false, Some("TRUE")));
        assert!(quiet_from_env(false, Some("on")));
    }

    #[test]
    fn quiet_drops_input_by_default() {
        assert!(input_dropped_from_env(true, None));
        assert!(input_dropped_from_env(true, Some("0")));
    }

    #[test]
    fn quiet_can_keep_input_live() {
        assert!(!input_dropped_from_env(true, Some("1")));
        assert!(!input_dropped_from_env(true, Some("TRUE")));
        assert!(!input_dropped_from_env(true, Some("on")));
        assert!(!input_dropped_from_env(false, None));
    }

    #[test]
    fn explicit_input_off_drops_edges_without_quiet() {
        assert!(input_dropped_from_env(false, Some("0")));
        assert!(input_dropped_from_env(false, Some("off")));
        assert!(!input_dropped_from_env(false, Some("1")));
    }
}
