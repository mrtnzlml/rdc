pub mod api;
pub mod cli;
pub mod config;
pub mod log;
pub mod mapping;
pub mod model;
pub mod overlay;
pub mod paths;
pub mod secrets;
pub mod slug;
pub mod snapshot;
pub mod state;
pub mod upgrade;

/// rdc's package version (compile-time from CARGO_PKG_VERSION).
/// Exposed for embedders (e.g. the Rossum Local desktop app).
pub fn version() -> Option<&'static str> {
    Some(env!("CARGO_PKG_VERSION"))
}
