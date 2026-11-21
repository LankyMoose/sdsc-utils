//! Browser app UI state and (later) its standalone window view.
//!
//! Phase 0 of the hub pivot keeps rendering inside Start; only the state moved
//! here so it no longer belongs to `StartState`. See
//! `notes/browser-hub-pivot.md`.

pub mod state;

pub use state::*;
