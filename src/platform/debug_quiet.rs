//! Debug-only "quiet" UI mode for screenshotting while the desktop is in use.
//!
//! Enabled when `SDSC_DEBUG_OPEN` is set (debug builds only), unless
//! `SDSC_DEBUG_QUIET=0` explicitly opts out. While quiet: windows never take
//! focus and are relocated from the primary monitor to a secondary one
//! (`ui::layout::{focus_window, show_window, move_window}`), pad edges are
//! dropped, and UI sounds / Start haptics are muted — so a debug session
//! cannot interrupt a game running on the primary monitor. Set
//! `SDSC_DEBUG_INPUT=1` to keep pad edges live in quiet mode (the install QA
//! console uses this to drive the hub on the secondary monitor while a game
//! keeps focus on the primary). Release builds always return `false`.

/// Env var that opens windows at boot; turns quiet mode on by default.
pub const DEBUG_OPEN_ENV: &str = "SDSC_DEBUG_OPEN";

/// Opt out of quiet mode while keeping [`DEBUG_OPEN_ENV`] auto-open.
/// Set to `0`, `false`, or `off`.
pub const DEBUG_QUIET_ENV: &str = "SDSC_DEBUG_QUIET";

/// Keep pad edges live while quiet mode still owns window placement/focus.
/// Set to `1`, `true`, or `on`.
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

/// True when quiet mode should swallow controller edges.
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
    if !open {
        return false;
    }
    !matches!(
        quiet.map(str::to_ascii_lowercase).as_deref(),
        Some("0" | "false" | "off")
    )
}

#[cfg(any(debug_assertions, test))]
fn input_dropped_from_env(quiet: bool, input: Option<&str>) -> bool {
    quiet
        && !matches!(
            input.map(str::to_ascii_lowercase).as_deref(),
            Some("1" | "true" | "on")
        )
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
    fn no_auto_open_is_never_quiet() {
        assert!(!quiet_from_env(false, None));
        assert!(!quiet_from_env(false, Some("1")));
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
}
