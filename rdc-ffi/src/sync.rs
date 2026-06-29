//! The one operation that needs rdc's async sync engine. Runs `sync_no_push`
//! on a fresh current-thread Tokio runtime (the engine holds `!Send` types
//! across awaits, so it cannot run on a multi-thread worker).

use crate::discover::count_files;
use crate::error::FfiError;
use std::future::Future;
use std::path::PathBuf;

/// Block the calling thread on `fut` using a current-thread runtime.
pub fn block_on<F: Future>(fut: F) -> F::Output {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("build current-thread tokio runtime")
        .block_on(fut)
}

#[derive(Debug, Clone, uniffi::Enum)]
pub enum SyncPhase {
    Started,
    Done { file_count: u64 },
    Error { message: String },
}

#[uniffi::export(callback_interface)]
pub trait SyncProgress: Send + Sync {
    fn on_phase(&self, phase: SyncPhase);
}

#[derive(Debug, Clone, uniffi::Record)]
pub struct SyncResult {
    pub file_count: u64,
}

/// Pull-only sync of one Connection. Scaffolds init files, resolves the
/// token (silent re-login in password mode), then runs `sync_no_push`.
#[uniffi::export]
pub fn sync_connection(
    folder: String,
    api_base: String,
    org_id: u64,
    progress: Box<dyn SyncProgress>,
) -> Result<SyncResult, FfiError> {
    let folder = PathBuf::from(folder);
    progress.on_phase(SyncPhase::Started);

    let result: anyhow::Result<u64> = block_on(async {
        rdc::cli::init::write_scaffold_files(&folder, "main", &api_base, org_id)?;
        let token = rdc::secrets::resolve_token(&folder, "main", &api_base).await?;
        rdc::cli::sync::embed::sync_no_push(&folder, "main", &token).await?;
        Ok(count_files(&folder.join("envs/main")))
    });

    match result {
        Ok(file_count) => {
            progress.on_phase(SyncPhase::Done { file_count });
            Ok(SyncResult { file_count })
        }
        Err(e) => {
            let message = format!("{e:#}");
            progress.on_phase(SyncPhase::Error { message: message.clone() });
            Err(FfiError::Operation { message })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn block_on_runs_async_to_completion() {
        let n = block_on(async { 20 + 22 });
        assert_eq!(n, 42);
    }

    struct NoopProgress;
    impl SyncProgress for NoopProgress {
        fn on_phase(&self, _phase: SyncPhase) {}
    }

    // Live test: requires a reachable Rossum env + valid token in the project.
    // Run manually with:  cargo test -p rdc-ffi -- --ignored live_sync
    // Set RDC_FFI_LIVE_DIR to a folder containing rdc.toml + secrets/main.secrets.json.
    #[test]
    #[ignore = "hits the live Rossum API; set RDC_FFI_LIVE_DIR and run with --ignored"]
    fn live_sync_pulls_files() {
        let dir = std::env::var("RDC_FFI_LIVE_DIR").expect("set RDC_FFI_LIVE_DIR");
        let toml = std::fs::read_to_string(std::path::Path::new(&dir).join("rdc.toml")).unwrap();
        let parsed: toml::Value = toml::from_str(&toml).unwrap();
        let env = &parsed["envs"]["main"];
        let api_base = env["api_base"].as_str().unwrap().to_string();
        let org_id = env["org_id"].as_integer().unwrap() as u64;

        let result = sync_connection(dir, api_base, org_id, Box::new(NoopProgress)).unwrap();
        assert!(result.file_count > 0, "expected pulled files");
    }
}
