//! Typed error channel for fallible app operations.
//!
//! Leaf module: `std`-only, with no `crate::` imports, so `build.rs` (which
//! compiles `src/ui/svg_icon.rs` via `#[path]` but cannot link `hidapi`)
//! can share this exact file instead of drifting twins. The
//! `From<hidapi::HidError>` impl lives in `crate` root (`src/lib.rs`).
//!
//! Each variant carries the fully-formatted message as a `String` and
//! [`AppError`]'s [`Display`](std::fmt::Display) renders it verbatim, so log
//! text is byte-identical to the old `Result<T, String>` channel. Split
//! `{label, message}` payloads were rejected for that reason: the existing
//! messages have heterogeneous shapes (`"failed to create dir for {path}:
//! {err}"`, `"open exited with {status}"`, ...) that no single `Display`
//! template can reproduce.

use std::fmt;

/// Fallible-operation error, typed by producing domain.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AppError {
    /// `controller/hid` + `controller/dualsense` (poll, lightbar, rumble,
    /// power-off, transport, `HidApi`/`HidError` conversions, HID lock).
    Hid(String),
    /// `persist` (atomic store writes, data-dir open).
    Persist(String),
    /// `platform` (autostart, crash-restart budget, `std::io` conversions).
    Platform(String),
    /// `games` (Steam scan/root, launch/close).
    Games(String),
    /// Spectrum / color config (`domain/color`) and CLI color values.
    Config(String),
    /// UI raster (`ui/svg_icon`) and app task plumbing (`app::spawn_blocking`).
    Ui(String),
}

impl fmt::Display for AppError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Hid(msg)
            | Self::Persist(msg)
            | Self::Platform(msg)
            | Self::Games(msg)
            | Self::Config(msg)
            | Self::Ui(msg) => write!(f, "{msg}"),
        }
    }
}

impl std::error::Error for AppError {}

/// Bare `?` on `std::io` results (no added context). Only `platform` code
/// uses this; `persist` keeps its explicit `"failed to ...: {e}"` context.
impl From<std::io::Error> for AppError {
    fn from(err: std::io::Error) -> Self {
        Self::Platform(err.to_string())
    }
}

/// Fallible-operation alias for the app's typed error channel.
pub type AppResult<T> = Result<T, AppError>;

#[cfg(test)]
mod tests {
    use super::*;

    /// Log-text contract: `Display` must echo the payload verbatim so every
    /// `app_log::warn(format!("...: {err}"))` line is byte-identical to the
    /// old `Result<T, String>` channel.
    #[test]
    fn display_echoes_payload_verbatim() {
        let cases = [
            AppError::Hid("hid boom".into()),
            AppError::Persist("persist boom".into()),
            AppError::Platform("platform boom".into()),
            AppError::Games("games boom".into()),
            AppError::Config("config boom".into()),
            AppError::Ui("ui boom".into()),
        ];
        let labels = ["hid", "persist", "platform", "games", "config", "ui"];
        for (err, label) in cases.into_iter().zip(labels) {
            assert_eq!(err.to_string(), format!("{label} boom"), "{label:?}");
        }
    }

    #[test]
    fn from_io_preserves_text_and_domain() {
        let expected = std::io::Error::new(std::io::ErrorKind::NotFound, "nope").to_string();
        let err = AppError::from(std::io::Error::new(std::io::ErrorKind::NotFound, "nope"));
        assert!(matches!(err, AppError::Platform(_)));
        assert_eq!(err.to_string(), expected);
    }
}
