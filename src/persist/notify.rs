//! Backward-compatible re-export: connect/low-battery/charge-complete edge
//! detection now lives in [`crate::session::notify`] (the session owns
//! presence + toasts). Existing `persist::notify` paths keep working.

pub use crate::session::notify::{NotifyEvent, NotifyTracker};
