//! Package identity derived from Cargo.toml (`package.name` / version).

pub const PKG_NAME: &str = env!("CARGO_PKG_NAME");
pub const PKG_VERSION: &str = env!("CARGO_PKG_VERSION");
pub const DISPLAY_NAME: &str = "SDSC Utils";

/// Undocumented internal argv: relaunch-for-replace (renderer-pref reload).
/// The parent is still exiting and holds the single-instance guard, so the
/// child retries acquiring it briefly instead of quitting as a duplicate.
pub const REPLACE_FLAG: &str = "--replace";

/// Undocumented internal argv: login launch via the Startup entry.
/// The autostart shortcut passes this so a login boot can stay quiet
/// (no startup notice toast); manual launches never carry it.
pub const LOGIN_FLAG: &str = "--started-at-login";
