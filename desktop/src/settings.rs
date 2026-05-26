use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

const SETTINGS_VERSION: u32 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum UpdateChannel {
    Stable,
    Beta,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Settings {
    pub version: u32,
    pub default_folder_parent: PathBuf,
    #[serde(default = "default_channel")]
    pub update_channel: UpdateChannel,
}

fn default_channel() -> UpdateChannel {
    UpdateChannel::Stable
}

impl Settings {
    pub fn load(path: &Path) -> Result<Self> {
        match std::fs::read(path) {
            Ok(bytes) => {
                let s: Settings = serde_json::from_slice(&bytes)
                    .with_context(|| format!("parsing {}", path.display()))?;
                if s.version != SETTINGS_VERSION {
                    anyhow::bail!(
                        "unsupported settings version {} (expected {})",
                        s.version,
                        SETTINGS_VERSION
                    );
                }
                Ok(s)
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::defaults()),
            Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
        }
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        let bytes = serde_json::to_vec_pretty(self).context("serializing settings")?;
        rdc::snapshot::writer::write_atomic(path, &bytes)
            .with_context(|| format!("writing {}", path.display()))?;
        Ok(())
    }

    pub fn defaults() -> Self {
        let parent = crate::paths::default_folder_parent()
            .unwrap_or_else(|_| PathBuf::from("Rossum"));
        Self {
            version: SETTINGS_VERSION,
            default_folder_parent: parent,
            update_channel: UpdateChannel::Stable,
        }
    }
}
