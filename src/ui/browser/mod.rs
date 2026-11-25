//! Browser add-on panel state and window view.
//!
//! These types are rendered by `sdsc-browser`, not the main shell; the child
//! owns the page surface, cursor, and text session. See
//! `notes/browser-app-split.md`.

pub mod state;
pub mod view;

pub use state::*;
pub use view::BrowserMessage;
