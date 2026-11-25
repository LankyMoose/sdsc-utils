//! Browser app UI state and its standalone window view.
//!
//! v0 has one header row (Back / URL / Forward) above the page. See
//! `notes/browser-embedding.md` for the engine/cursor contract and
//! `notes/browser-home-pivot.md` for window ownership.

pub mod state;
pub mod view;

pub use state::*;
pub use view::BrowserMessage;
