use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::Path;

#[derive(Debug, Serialize, Deserialize, PartialEq, Clone)]
pub struct ProjectConfig {
    pub project: ProjectMeta,
    #[serde(default)]
    pub envs: BTreeMap<String, EnvConfig>,
}

#[derive(Debug, Serialize, Deserialize, PartialEq, Clone)]
pub struct ProjectMeta {
    pub name: String,
}

#[derive(Debug, Serialize, Deserialize, PartialEq, Clone)]
pub struct EnvConfig {
    pub api_base: String,
    pub org_id: u64,
    #[serde(default)]
    pub workspace_filter: Option<String>,
}

impl ProjectConfig {
    pub fn load(path: &Path) -> Result<Self> {
        let raw = std::fs::read_to_string(path)
            .with_context(|| format!("reading {}", path.display()))?;
        let cfg: ProjectConfig = toml::from_str(&raw)
            .with_context(|| format!("parsing {}", path.display()))?;
        Ok(cfg)
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        let s = toml::to_string_pretty(self)
            .context("serializing project config")?;
        crate::snapshot::writer::write_atomic(path, s.as_bytes())?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn example() -> ProjectConfig {
        let mut envs = BTreeMap::new();
        envs.insert(
            "dev".to_string(),
            EnvConfig {
                api_base: "https://example.rossum.app/api/v1".to_string(),
                org_id: 285704,
                workspace_filter: None,
            },
        );
        ProjectConfig {
            project: ProjectMeta { name: "demo".to_string() },
            envs,
        }
    }

    #[test]
    fn round_trip_to_disk() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("rdc.toml");
        example().save(&path).unwrap();
        let loaded = ProjectConfig::load(&path).unwrap();
        assert_eq!(loaded, example());
    }

    #[test]
    fn missing_file_errors_with_path() {
        let err = ProjectConfig::load(Path::new("/nope/rdc.toml")).unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("/nope/rdc.toml"), "error should name the path: {msg}");
    }
}
