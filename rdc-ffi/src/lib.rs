//! FFI bridge from the native macOS app ("Rossum Local") into the rdc core.
//!
//! Every operation re-uses rdc's own functions; this crate adds no new
//! credential or sync logic. See `docs/superpowers/specs/2026-06-29-native-macos-app-design.md`.

/// rdc's package version, surfaced to the app's About box.
pub fn version() -> Option<&'static str> {
    rdc::version()
}
