//! Embedding entry point for non-CLI consumers (e.g. the Rossum Local
//! macOS app).
//!
//! Bypasses two CLI-only assumptions:
//! - `std::env::current_dir()` for locating `rdc.toml` and the snapshot.
//! - On-disk `secrets/<env>.secrets.json` for the API token.
//!
//! The caller supplies both explicitly. Everything else (fetch,
//! decode, atomic write, lockfile, `_index.md`) reuses the existing
//! sync pipeline in no-push, non-interactive mode.

use crate::cli::sync::CycleOutcome;
use anyhow::Result;
use std::path::Path;

/// Run one no-push reconciliation cycle.
///
/// - `cwd`: project root containing `rdc.toml`.
/// - `env`: env name (the desktop app always uses `"main"`).
/// - `token`: pre-resolved API token; the secrets file is not touched.
///
/// Returns `CycleOutcome` with per-class counts. Errors propagate as
/// `anyhow::Error`; the caller surfaces them to the user.
pub async fn sync_no_push(cwd: &Path, env: &str, token: &str) -> Result<CycleOutcome> {
    let paths = crate::paths::Paths::for_env(cwd, env);
    let _lock = crate::cli::sync::lock::EnvLock::acquire(
        &paths.env_lock(),
        std::time::Duration::from_secs(30),
    )?;
    crate::cli::sync::run_cycle(
        env,
        false, // interactive
        false, // dry_run
        false, // allow_deletes
        true,  // no_push  <-- the embedding contract
        false, // no_pull
        None,
        Some(cwd),
        Some(token.to_string()),
    )
    .await
}
