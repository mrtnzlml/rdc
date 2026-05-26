use crate::diagnostics::DiagLog;
use crate::keychain::macos::MacOsKeychain;
use crate::registry::Registry;
use crate::settings::Settings;
use crate::sync_queue::SyncQueue;
use anyhow::Result;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::Mutex;

pub struct AppState {
    pub registry: Arc<Mutex<Registry>>,
    pub settings: Arc<Mutex<Settings>>,
    pub registry_path: PathBuf,
    pub settings_path: PathBuf,
    pub keychain: Arc<MacOsKeychain>,
    pub queue: SyncQueue,
    pub diag: Arc<DiagLog>,
}

impl AppState {
    pub fn load() -> Result<Self> {
        let registry_path = crate::paths::registry_path()?;
        let settings_path = crate::paths::settings_path()?;
        let registry = Registry::load(&registry_path)?;
        let settings = Settings::load(&settings_path)?;
        Ok(Self {
            registry: Arc::new(Mutex::new(registry)),
            settings: Arc::new(Mutex::new(settings)),
            registry_path,
            settings_path,
            keychain: Arc::new(MacOsKeychain),
            queue: SyncQueue::new(4),
            diag: Arc::new(DiagLog::default()),
        })
    }
}
