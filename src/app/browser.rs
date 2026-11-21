//! Top-level Browser app state.
//!
//! Phase 0 of the hub pivot lifts the browser out of `StartState`: the panel,
//! engine handle, capability flag, and browser-only D-pad stepper live here so
//! the browser no longer disappears when the Start screen is not the owner.
//! Behavior is unchanged — Start still renders the panel until the standalone
//! Browser window lands. See `notes/browser-hub-pivot.md`.

use crate::browser::engine::EngineHandle;
use crate::domain::pad::NavStepper;
use crate::ui::browser::BrowserPanel;

#[derive(Default)]
pub struct BrowserAppState {
    /// Tabs, chrome targets, cursor, bookmarks, and per-tab page state.
    pub panel: BrowserPanel,
    /// Offscreen engine worker (`servo-engine` today, CEF later). `None` when
    /// this build/runtime has no engine backend.
    pub engine: Option<EngineHandle>,
    /// Process-static capability: true when an engine worker is live. Drives
    /// the page placeholder copy and, later, the hub tile install state.
    pub engine_available: bool,
    /// Engine-mode D-pad cursor jumps run on this standalone stepper (fed a
    /// stickless `dpad_only` mask): the Start bank stays parked in engine
    /// mode so stick moves never double-fire into both cursor and nav.
    pub dpad_stepper: NavStepper,
}
