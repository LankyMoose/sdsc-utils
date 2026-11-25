//! Browser app UI state and (later) its standalone window view.
//!
//! Phase 0 of Home pivot keeps rendering inside Launcher; only the state moved
//! here so it no longer belongs to `LauncherState`. See
//! `notes/browser-Home-pivot.md`.

pub mod state;
pub mod view;

pub use state::*;
pub use view::BrowserMessage;
