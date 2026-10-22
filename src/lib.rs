//! SDSC Utils shared library — domain, HID, and session.

/// Fallible-operation alias for the app's stringly-typed error channel.
///
/// HID/`std::io` errors keep their own `Result<_, hidapi::HidError>` /
/// `Result<_, std::io::Error>` types; only `String`-err results use this.
pub type AppResult<T> = Result<T, String>;

pub mod app;
pub mod controller;
pub mod domain;
pub mod games;
pub mod persist;
pub mod platform;
pub mod session;
pub mod ui;
