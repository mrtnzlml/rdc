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
    // Scaffold init-time files (CLAUDE.md, README.md, .gitignore,
    // .gitattributes) on first sync. Each writer skips when its target
    // exists, so this is cheap and self-healing if a file gets deleted.
    rdc::cli::init::write_scaffold_files(&conn.folder, "main", &conn.api_base, conn.org_id)
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
    if msg.contains("timed out") && msg.contains("env lock") {
        SyncError::LockContended
    } else {
        classify_io_err(e, folder)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classify_rdc_err_recognizes_lock_timeout_message() {
        let err = anyhow::anyhow!("timed out after 30s waiting for env lock at /tmp/foo");
        let classified = classify_rdc_err(err, std::path::Path::new("/tmp/foo"));
        assert!(matches!(classified, SyncError::LockContended), "got: {classified:?}");
    }

    #[test]
    fn classify_io_err_recognizes_disk_full_message() {
        let err = anyhow::anyhow!("No space left on device");
        let classified = classify_io_err(err, std::path::Path::new("/tmp/foo"));
        assert!(matches!(classified, SyncError::DiskFull(_)), "got: {classified:?}");
    }

    #[test]
    fn classify_io_err_falls_back_to_other() {
        let err = anyhow::anyhow!("unrelated random error");
        let classified = classify_io_err(err, std::path::Path::new("/tmp/foo"));
        assert!(matches!(classified, SyncError::Other(_)), "got: {classified:?}");
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
