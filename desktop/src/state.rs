//! Process-wide app state. Holds two pieces:
//! - The parent directory under which Connection folders live
//!   (resolved at startup, override via `ROSSUM_LOCAL_PARENT`).
//! - A `Mutex<()>` serializing sync operations — Tauri's command
//!   handlers can be invoked concurrently and rdc holds an env lock
//!   per project, so we keep our own ordering simple by queueing
//!   one sync at a time across all Connections.

use crate::discover;
use std::path::PathBuf;
use std::sync::atomic::AtomicUsize;
use std::sync::Arc;
use tokio::sync::Mutex;

pub struct AppState {
    pub parent: PathBuf,
    pub sync_lock: Mutex<()>,
    /// Count of in-flight sync_connection invocations. Used to set the
    /// Dock-icon badge (`set_badge_count`) so the user sees activity
    /// even when the app is hidden.
    pub active_syncs: Arc<AtomicUsize>,
}

impl AppState {
    pub fn load() -> anyhow::Result<Self> {
        let parent = discover::parent_default()?;
        std::fs::create_dir_all(&parent)?;
        Ok(Self {
            parent,
            sync_lock: Mutex::new(()),
            active_syncs: Arc::new(AtomicUsize::new(0)),
        })
    }
}
