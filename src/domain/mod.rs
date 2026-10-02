//! Shared domain types below the UI and HID layers.
//!
//! Color, pad samples, gestures, and DualSense protocol constants live here so
//! `controller` and `persist` never import widget modules.

pub mod color;
pub mod gesture;
pub mod pad;
pub mod protocol;
