//! Remembered-controller store alias: canonical types live in
//! [`crate::controller::known`]; re-exported here so all persisted state is
//! reachable through `persist`.

pub use crate::controller::known::{KnownController, KnownControllers};
