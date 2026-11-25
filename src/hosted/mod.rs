//! Hosted app contract v0.
//!
//! The versioned types live in the shared `sdsc-browser-protocol` crate so the
//! same contract can cross an in-process adapter or a named pipe after the
//! Browser app split. This module is the main app's import surface.

pub use sdsc_browser_protocol::*;
