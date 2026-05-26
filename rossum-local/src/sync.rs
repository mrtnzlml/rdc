use crate::auth::{resolve_token_for_sync, ResolveError};
use crate::connection::Connection;
use crate::keychain::Keychain;
use crate::rdc_toml::ensure_rdc_toml;
use thiserror::Error;

#[derive(Debug, Clone)]
pub struct SyncOutcome {
    pub file_count: u64,
}

#[derive(Debug, Error)]
pub enum SyncError {
    #[error(transparent)]
    Auth(#[from] ResolveError),
    #[error("Could not write to {0}. Make space and try again.")]
    DiskFull(String),
    #[error("Folder is in use by another rdc process. Try again.")]
    LockContended,
    #[error("{0}")]
    Other(String),
}

pub async fn run_sync<K: Keychain + ?Sized>(
    conn: &Connection,
    kc: &K,
) -> Result<SyncOutcome, SyncError> {
    let ts = resolve_token_for_sync(conn, kc).await?;
    ensure_rdc_toml(&conn.folder, &conn.api_base, conn.org_id)
        .map_err(|e| classify_io_err(e, &conn.folder))?;

    rdc::cli::sync::embed::sync_no_push(&conn.folder, "main", &ts.token)
        .await
        .map_err(|e| classify_rdc_err(e, &conn.folder))?;

    let file_count = count_snapshot_files(&conn.folder);
    Ok(SyncOutcome { file_count })
}

fn classify_io_err(e: anyhow::Error, folder: &std::path::Path) -> SyncError {
    let msg = format!("{e:#}");
    if msg.contains("No space left") {
        SyncError::DiskFull(folder.display().to_string())
    } else {
        SyncError::Other(msg)
    }
}

fn classify_rdc_err(e: anyhow::Error, folder: &std::path::Path) -> SyncError {
    let msg = format!("{e:#}");
    if msg.contains("lock") && msg.contains("contend") {
        SyncError::LockContended
    } else {
        classify_io_err(e, folder)
    }
}

fn count_snapshot_files(folder: &std::path::Path) -> u64 {
    fn walk(p: &std::path::Path, acc: &mut u64) {
        if let Ok(rd) = std::fs::read_dir(p) {
            for entry in rd.flatten() {
                let path = entry.path();
                if path.file_name().map(|n| n == ".rdc").unwrap_or(false) {
                    continue;
                }
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
    walk(&folder.join("envs/main"), &mut n);
    n
}
