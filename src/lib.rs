//! SDSC Utils shared library — domain, HID, and session.

/// Typed fallible-operation error channel (see [`app_error`]).
///
/// HID/`std::io` errors convert via `From` where the call site adds no
/// context; every other site wraps its existing message string in the
/// producing domain's variant, so `Display` text is unchanged.
pub mod app_error;
pub use app_error::{AppError, AppResult};

/// Bare `?` on HID results (no added context). Lives here — not in
/// [`app_error`] — because `build.rs` shares that leaf file via `#[path]`
/// and cannot link `hidapi`. Stores the `HidError` text, so `Display`
/// matches the old `.map_err(|e| e.to_string())?` sites exactly.
impl From<hidapi::HidError> for AppError {
    fn from(err: hidapi::HidError) -> Self {
        Self::Hid(err.to_string())
    }
}

pub mod app;
pub mod controller;
pub mod domain;
pub mod games;
pub mod persist;
pub mod platform;
pub mod session;
pub mod ui;
