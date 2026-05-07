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

impl EnvConfig {
    /// Derive the Data Storage (MDH) service base URL from `api_base`.
    ///
    /// API and Data Storage share the same parent domain on every Rossum
    /// cluster we've seen, but the API host is prefixed with `api.` (or
    /// the path with `/api`), while Data Storage sits at the bare parent
    /// domain plus a service path. Examples:
    ///
    /// | api_base                                  | derived data storage base                       |
    /// |-------------------------------------------|-------------------------------------------------|
    /// | `https://api.elis.rossum.ai/v1`           | `https://elis.rossum.ai/svc/data-storage/api`   |
    /// | `https://customer.rossum.app/api/v1`      | `https://customer.rossum.app/svc/data-storage/api` |
    /// | `http://127.0.0.1:54321/api/v1`           | `http://127.0.0.1:54321/svc/data-storage/api`   |
    ///
    /// If MDH isn't enabled on the target cluster, the resulting URL
    /// will return 404 — the pull driver tolerates that.
    pub fn data_storage_base(&self) -> String {
        derive_data_storage_base(&self.api_base)
    }
}

/// Pure-fn derivation. Strips an `api.` host-subdomain prefix and an
/// `/api/v1` or `/v1` path suffix, then appends `/svc/data-storage/api`.
fn derive_data_storage_base(api_base: &str) -> String {
    let trimmed = api_base.trim_end_matches('/');
    let (scheme, rest) = match trimmed.split_once("://") {
        Some((s, r)) => (s, r),
        None => ("https", trimmed),
    };
    let (host, path) = match rest.split_once('/') {
        Some((h, p)) => (h, format!("/{p}")),
        None => (rest, String::new()),
    };
    let host = host.strip_prefix("api.").unwrap_or(host);
    // Be liberal: try the more specific pattern first.
    let path = path
        .trim_end_matches("/api/v1")
        .trim_end_matches("/v1")
        .trim_end_matches('/');
    format!("{scheme}://{host}{path}/svc/data-storage/api")
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

    #[test]
    fn data_storage_url_from_api_subdomain_form() {
        // api. subdomain prefix, /v1 path
        assert_eq!(
            derive_data_storage_base("https://api.elis.rossum.ai/v1"),
            "https://elis.rossum.ai/svc/data-storage/api",
        );
    }

    #[test]
    fn data_storage_url_from_path_prefix_form() {
        // bare host, /api/v1 path (the customer.rossum.app pattern)
        assert_eq!(
            derive_data_storage_base("https://customer.rossum.app/api/v1"),
            "https://customer.rossum.app/svc/data-storage/api",
        );
    }

    #[test]
    fn data_storage_url_from_loopback() {
        // Used by integration tests with mock servers.
        assert_eq!(
            derive_data_storage_base("http://127.0.0.1:54321/api/v1"),
            "http://127.0.0.1:54321/svc/data-storage/api",
        );
    }

    #[test]
    fn data_storage_url_handles_trailing_slash() {
        assert_eq!(
            derive_data_storage_base("https://api.elis.rossum.ai/v1/"),
            "https://elis.rossum.ai/svc/data-storage/api",
        );
    }

    #[test]
    fn data_storage_url_handles_both_api_subdomain_and_path() {
        // Unusual but parsed liberally.
        assert_eq!(
            derive_data_storage_base("https://api.foo.com/api/v1"),
            "https://foo.com/svc/data-storage/api",
        );
    }

    #[test]
    fn env_config_data_storage_base_uses_derivation() {
        let cfg = EnvConfig {
            api_base: "https://api.elis.rossum.ai/v1".into(),
            org_id: 1,
            workspace_filter: None,
        };
        assert_eq!(cfg.data_storage_base(), "https://elis.rossum.ai/svc/data-storage/api");
    }
}
