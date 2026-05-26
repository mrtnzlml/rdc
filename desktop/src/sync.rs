//! Sync orchestrator — the thinnest possible wrapper around rdc.
//!
//! 1. Scaffold init-time files (CLAUDE.md, README.md, .gitignore,
//!    .gitattributes) if missing. Each writer is idempotent.
//! 2. Resolve the API token via `rdc::secrets::resolve_token`. This
//!    handles the cached token, the silent re-login from file-stored
//!    credentials (password mode), and the env-var fallback — all
//!    identical to what the rdc CLI does.
//! 3. Call `rdc::cli::sync::embed::sync_no_push` for the actual pull.
//!
//! No authentication code, no credential storage, no progress-event
//! plumbing beyond what the caller emits around this function.

use anyhow::{Context, Result};
use std::path::Path;

pub async fn run_sync(folder: &Path, api_base: &str, org_id: u64) -> Result<u64> {
    rdc::cli::init::write_scaffold_files(folder, "main", api_base, org_id)
        .context("writing scaffold files")?;

    let token = rdc::secrets::resolve_token(folder, "main", api_base)
        .await
        .context("resolving credentials")?;

    rdc::cli::sync::embed::sync_no_push(folder, "main", &token)
        .await
        .context("running sync")?;

    Ok(count_files(&folder.join("envs/main")))
}

fn count_files(p: &Path) -> u64 {
    fn walk(p: &Path, acc: &mut u64) {
        if let Ok(rd) = std::fs::read_dir(p) {
            for entry in rd.flatten() {
                let path = entry.path();
                let Ok(meta) = entry.metadata() else { continue };
                if meta.is_dir() {
                    walk(&path, acc);
                } else if meta.is_file() {
                    *acc += 1;
                }
            }
        }
    }
    let mut n = 0;
    walk(p, &mut n);
    n
}
