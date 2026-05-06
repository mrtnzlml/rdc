use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::Path;

pub const LOCKFILE_VERSION: u32 = 1;

#[derive(Debug, Serialize, Deserialize, PartialEq, Clone)]
pub struct Lockfile {
    pub version: u32,
    /// Per object-type, a map of slug -> entry.
    pub objects: BTreeMap<String, BTreeMap<String, ObjectEntry>>,
}

#[derive(Debug, Serialize, Deserialize, PartialEq, Clone)]
pub struct ObjectEntry {
    pub id: u64,
    /// ISO 8601 timestamp from the server (`modified_at`), if present.
    #[serde(default)]
    pub modified_at: Option<String>,
}

impl Default for Lockfile {
    fn default() -> Self {
        Self { version: LOCKFILE_VERSION, objects: BTreeMap::new() }
    }
}

impl Lockfile {
    pub fn load(path: &Path) -> Result<Self> {
        if !path.exists() {
            return Ok(Self::default());
        }
        let raw = std::fs::read_to_string(path)
            .with_context(|| format!("reading {}", path.display()))?;
        let lf: Lockfile = serde_json::from_str(&raw)
            .with_context(|| format!("parsing {}", path.display()))?;
        if lf.version != LOCKFILE_VERSION {
            anyhow::bail!(
                "lockfile {} has version {} but this rdc supports {}",
                path.display(),
                lf.version,
                LOCKFILE_VERSION
            );
        }
        Ok(lf)
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        let s = serde_json::to_string_pretty(self)
            .context("serializing lockfile")?;
        crate::snapshot::writer::write_atomic(path, format!("{s}\n").as_bytes())?;
        Ok(())
    }

    pub fn upsert(&mut self, kind: &str, slug: &str, entry: ObjectEntry) {
        self.objects
            .entry(kind.to_string())
            .or_default()
            .insert(slug.to_string(), entry);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn missing_file_returns_empty() {
        let lf = Lockfile::load(Path::new("/nope.json")).unwrap();
        assert_eq!(lf, Lockfile::default());
    }

    #[test]
    fn round_trip() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("dev.lock.json");
        let mut lf = Lockfile::default();
        lf.upsert("hooks", "validator-invoices", ObjectEntry {
            id: 1,
            modified_at: Some("2026-04-01T10:00:00Z".to_string()),
        });
        lf.save(&path).unwrap();
        let loaded = Lockfile::load(&path).unwrap();
        assert_eq!(loaded, lf);
    }

    #[test]
    fn future_version_errors() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("dev.lock.json");
        std::fs::write(&path, r#"{"version":999,"objects":{}}"#).unwrap();
        let err = Lockfile::load(&path).unwrap_err();
        assert!(format!("{err:#}").contains("version"));
    }
}
