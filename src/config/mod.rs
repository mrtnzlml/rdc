use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::Path;

#[derive(Debug, Serialize, Deserialize, PartialEq, Clone, Default)]
pub struct ProjectConfig {
    #[serde(default)]
    pub envs: BTreeMap<String, EnvConfig>,
}

#[derive(Debug, Serialize, Deserialize, PartialEq, Clone)]
pub struct EnvConfig {
    pub api_base: String,
    pub org_id: u64,
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
        let raw = match std::fs::read_to_string(path) {
            Ok(s) => s,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                // Self-contained, actionable error. The most common reason
                // this hits is the user running an rdc command outside a
                // project tree — point them at `rdc init`. We swallow the
                // raw `os error 2` and the path-in-message stays short
                // (no nested "reading <path>: No such file …").
                let parent = path.parent().unwrap_or(path);
                return Err(anyhow::anyhow!(
                    "not an rdc project: no rdc.toml in {}.\n\
                     run `rdc init` here, or cd into an existing project directory.",
                    parent.display()
                ));
            }
            Err(e) => {
                return Err(anyhow::Error::new(e)
                    .context(format!("reading {}", path.display())));
            }
        };
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
            },
        );
        ProjectConfig { envs }
    }

    /// Older `rdc.toml` files written before the `[project]` section was
    /// removed must still load — serde ignores unknown fields by default,
    /// and `envs` is what we care about.
    #[test]
    fn load_ignores_legacy_project_section() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("rdc.toml");
        std::fs::write(
            &path,
            r#"[project]
name = "legacy"

[envs.dev]
api_base = "https://example.rossum.app/api/v1"
org_id = 285704
"#,
        )
        .unwrap();
        let cfg = ProjectConfig::load(&path).unwrap();
        assert!(cfg.envs.contains_key("dev"));
        assert_eq!(cfg.envs["dev"].org_id, 285704);
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
    fn missing_file_errors_actionable() {
        let err = ProjectConfig::load(Path::new("/nope/rdc.toml")).unwrap_err();
        let msg = format!("{err:#}");
        // The error must surface the missing directory + an actionable hint
        // toward `rdc init`, so a user running an rdc command outside a
        // project tree gets pointed at the fix instead of a raw `os error 2`.
        assert!(msg.contains("/nope"), "error should name the directory: {msg}");
        assert!(msg.contains("not an rdc project"), "error should explain what's wrong: {msg}");
        assert!(msg.contains("rdc init"), "error should suggest the fix: {msg}");
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
        };
        assert_eq!(cfg.data_storage_base(), "https://elis.rossum.ai/svc/data-storage/api");
    }
}
