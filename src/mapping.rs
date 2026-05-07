//! Env-pair mapping — connects src slug ↔ tgt slug per kind. Written by
//! `rdc map`, consumed by `rdc plan` / `rdc apply`. Per spec §10.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::Path;

#[derive(Debug, Serialize, Deserialize, PartialEq, Clone)]
pub struct Mapping {
    pub version: u32,
    #[serde(default)]
    pub hooks: BTreeMap<String, String>,
    #[serde(default)]
    pub rules: BTreeMap<String, String>,
    #[serde(default)]
    pub labels: BTreeMap<String, String>,
}

impl Default for Mapping {
    fn default() -> Self {
        Self {
            version: 1,
            hooks: BTreeMap::new(),
            rules: BTreeMap::new(),
            labels: BTreeMap::new(),
        }
    }
}

impl Mapping {
    pub fn load(path: &Path) -> Result<Self> {
        if !path.exists() {
            return Ok(Self::default());
        }
        let raw = std::fs::read_to_string(path)
            .with_context(|| format!("reading {}", path.display()))?;
        let m: Mapping = toml::from_str(&raw)
            .with_context(|| format!("parsing {}", path.display()))?;
        Ok(m)
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        let s = toml::to_string_pretty(self)
            .context("serializing mapping")?;
        crate::snapshot::writer::write_atomic(path, s.as_bytes())?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn load_returns_default_when_missing() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("nope.toml");
        let m = Mapping::load(&path).unwrap();
        assert_eq!(m, Mapping::default());
    }

    #[test]
    fn round_trip() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("test_to_prod.toml");
        let mut m = Mapping::default();
        m.hooks.insert("validator-invoices".into(), "validator-invoices".into());
        m.hooks.insert("sftp-import".into(), "sftp-import-prod".into());
        m.rules.insert("validation-rule".into(), "validation-rule".into());
        m.labels.insert("priority-high".into(), "priority-high".into());
        m.save(&path).unwrap();
        let loaded = Mapping::load(&path).unwrap();
        assert_eq!(loaded, m);
    }
}
