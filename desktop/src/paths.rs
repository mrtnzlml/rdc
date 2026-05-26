use anyhow::{Context, Result};
use std::path::PathBuf;

const BUNDLE_ID: &str = "ai.rossum.local";

/// `~/Library/Application Support/ai.rossum.local/`
pub fn app_support_dir() -> Result<PathBuf> {
    let dirs = directories::ProjectDirs::from("ai", "rossum", "local")
        .context("resolving Application Support directory")?;
    Ok(dirs.data_dir().to_path_buf())
}

pub fn registry_path() -> Result<PathBuf> {
    Ok(app_support_dir()?.join("connections.json"))
}

pub fn settings_path() -> Result<PathBuf> {
    Ok(app_support_dir()?.join("settings.json"))
}

/// `~/Documents/Rossum/` by default.
pub fn default_folder_parent() -> Result<PathBuf> {
    let home = directories::UserDirs::new()
        .context("resolving user directories")?;
    let docs = home
        .document_dir()
        .context("no Documents directory on this system")?;
    Ok(docs.join("Rossum"))
}

pub fn keychain_service() -> &'static str {
    BUNDLE_ID
}
