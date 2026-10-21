//! Game-catalog alias: canonical types live in [`crate::games`];
//! re-exported here so persisted state is reachable through `persist`.

pub use crate::games::{GameEntry, GamesCatalog};
