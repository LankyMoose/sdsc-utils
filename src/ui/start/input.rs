//! Re-export pad nav / snapshot types from the domain layer.
//!
//! HID sampling and exclusive-fullscreen helpers still live in [`crate::domain::pad`]
//! until the worker owns input edges (phase 4).
pub use crate::domain::pad::*;
