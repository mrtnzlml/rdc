//! FFI bridge from the native macOS app ("Rossum Local") into the rdc core.
//!
//! Every operation re-uses rdc's own functions; this crate adds no new
//! credential or sync logic. See `docs/superpowers/specs/2026-06-29-native-macos-app-design.md`.

mod error;

uniffi::setup_scaffolding!();

/// rdc's package version, surfaced to the app's About box.
pub fn version() -> Option<&'static str> {
    rdc::version()
}

/// FFI-visible version string (UniFFI cannot return `&'static str`).
#[uniffi::export]
pub fn ffi_version() -> Option<String> {
    rdc::version().map(|s| s.to_string())
}

#[cfg(test)]
mod tests {
    #[test]
    fn ffi_version_matches_rdc() {
        assert_eq!(super::ffi_version().as_deref(), rdc::version());
        assert!(super::ffi_version().is_some());
    }
}
