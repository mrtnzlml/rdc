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

use crate::cli::resolve::ConflictStrategy;
use crate::cli::sync::CycleOutcome;
use crate::log::Log;
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
        None,  // conflict_strategy (embedding never resolves BothDiverged interactively)
        None,
        Some(cwd),
        Some(token.to_string()),
    )
    .await
}

/// Like [`sync_no_push`], but streams rdc's rendered progress log into
/// `log_sink` — one `write` per line — so an embedder (the desktop app) can
/// show the real sync log instead of a synthesized one.
///
/// Lines carry rdc's normal ANSI color (`ColorMode::Color`) so the embedder can
/// preserve it; the log is non-TTY (scrollback-clean lines, no cursor redraws).
/// Because a renderer is supplied, `run_cycle` omits its own cycle-closing
/// summary event; the caller adds its own completion line.
pub async fn sync_no_push_logged(
    cwd: &Path,
    env: &str,
    token: &str,
    log_sink: Box<dyn std::io::Write + Send>,
) -> Result<CycleOutcome> {
    let paths = crate::paths::Paths::for_env(cwd, env);
    let _lock = crate::cli::sync::lock::EnvLock::acquire(
        &paths.env_lock(),
        std::time::Duration::from_secs(30),
    )?;
    let renderer = Log::for_sink(crate::cli::resolve::ColorMode::Color, log_sink);
    crate::cli::sync::run_cycle(
        env,
        false, // interactive
        false, // dry_run
        false, // allow_deletes
        true,  // no_push
        false, // no_pull
        None,  // conflict_strategy
        Some(renderer),
        Some(cwd),
        Some(token.to_string()),
    )
    .await
}

/// Run one `--no-pull` (deploy) reconciliation cycle, streaming rdc's rendered
/// log into `log_sink`. `dry_run` renders the plan and stops before executing.
/// `conflict` selects the non-interactive BothDiverged strategy; `allow_deletes`
/// permits local-tombstone → remote DELETE. Pull is never performed (local files
/// are never overwritten). Used by the desktop app's promote Push.
pub async fn sync_push_logged(
    cwd: &Path,
    env: &str,
    token: &str,
    conflict: Option<ConflictStrategy>,
    allow_deletes: bool,
    dry_run: bool,
    log_sink: Box<dyn std::io::Write + Send>,
) -> Result<CycleOutcome> {
    let paths = crate::paths::Paths::for_env(cwd, env);
    let _lock = crate::cli::sync::lock::EnvLock::acquire(
        &paths.env_lock(),
        std::time::Duration::from_secs(30),
    )?;
    let renderer = Log::for_sink(crate::cli::resolve::ColorMode::Color, log_sink);
    crate::cli::sync::run_cycle(
        env,
        false, // interactive
        dry_run,
        allow_deletes,
        false, // no_push
        true,  // no_pull  <-- deploy: push local, never overwrite local
        conflict,
        Some(renderer),
        Some(cwd),
        Some(token.to_string()),
    )
    .await
}
