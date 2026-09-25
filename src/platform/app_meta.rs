//! Package identity derived from Cargo.toml (`package.name` / version).

pub const PKG_NAME: &str = env!("CARGO_PKG_NAME");
pub const PKG_VERSION: &str = env!("CARGO_PKG_VERSION");
pub const DISPLAY_NAME: &str = "SDSC Utils";
