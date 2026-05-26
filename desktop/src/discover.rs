//! Connection discovery via directory scan.
//!
//! The desktop app no longer keeps a separate registry. A Connection is
//! anything under the configured parent directory (default
//! `~/Documents/Rossum/`) that looks like an rdc project: a folder
//! containing an `rdc.toml` with an `[envs.main]` section.
//!
//! All Connection state — name, api_base, org_id, last sync timestamp,
//! file count, auth kind — is derived from on-disk artifacts. There is
//! no `connections.json` to keep in sync with reality.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone)]
pub struct Connection {
    pub folder: PathBuf,
    pub api_base: String,
    pub org_id: u64,
    pub auth_kind: AuthKind,
    pub last_sync_unix: Option<i64>,
    pub file_count: u64,
}

impl Connection {
    pub fn name(&self) -> &str {
        self.folder
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("?")
    }

    /// Connection id used by the frontend. The folder name is unique
    /// within the parent directory and stable across syncs — no
    /// separate ULID needed.
    pub fn id(&self) -> &str {
        self.name()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AuthKind {
    Token,
    Password,
}

#[derive(Deserialize)]
struct RdcToml {
    envs: std::collections::BTreeMap<String, RdcEnvConfig>,
}

#[derive(Deserialize)]
struct RdcEnvConfig {
    api_base: String,
    org_id: u64,
}

/// `~/Documents/Rossum/` — where new Connections land by default. Used
/// at startup to seed the parent directory; can be overridden via the
/// `ROSSUM_LOCAL_PARENT` env var for tests.
pub fn parent_default() -> Result<PathBuf> {
    if let Ok(p) = std::env::var("ROSSUM_LOCAL_PARENT") {
        return Ok(PathBuf::from(p));
    }
    let home = directories::UserDirs::new().context("resolving user directories")?;
    let docs = home
        .document_dir()
        .context("no Documents directory on this system")?;
    Ok(docs.join("Rossum"))
}

/// Enumerate Connections under `parent`. Folders without a valid
/// `rdc.toml` are silently skipped — the user may have stray
/// directories there.
pub fn scan(parent: &Path) -> Vec<Connection> {
    let mut out = Vec::new();
    let Ok(rd) = std::fs::read_dir(parent) else {
        return out;
    };
    for entry in rd.flatten() {
        if let Some(conn) = inspect(&entry.path()) {
            out.push(conn);
        }
    }
    out.sort_by(|a, b| a.name().cmp(b.name()));
    out
}

/// Look up a single Connection by folder name within `parent`.
pub fn find(parent: &Path, name: &str) -> Option<Connection> {
    inspect(&parent.join(name))
}

fn inspect(folder: &Path) -> Option<Connection> {
    if !folder.is_dir() {
        return None;
    }
    let toml_path = folder.join("rdc.toml");
    let content = std::fs::read_to_string(&toml_path).ok()?;
    let parsed: RdcToml = toml::from_str(&content).ok()?;
    let env = parsed.envs.get("main")?;
    let secrets = rdc::secrets::read_secrets_file(folder, "main").unwrap_or_default();
    let auth_kind = if secrets.username.is_some() {
        AuthKind::Password
    } else {
        AuthKind::Token
    };
    let last_sync_unix = std::fs::metadata(folder.join(".rdc/state/main.lock.json"))
        .ok()
        .and_then(|m| m.modified().ok())
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs() as i64);
    let file_count = count_files(&folder.join("envs/main"));
    Some(Connection {
        folder: folder.to_path_buf(),
        api_base: env.api_base.clone(),
        org_id: env.org_id,
        auth_kind,
        last_sync_unix,
        file_count,
    })
}

fn count_files(p: &Path) -> u64 {
    fn walk(p: &Path, acc: &mut u64) {
        if let Ok(rd) = std::fs::read_dir(p) {
            for entry in rd.flatten() {
                let path = entry.path();
                let Ok(meta) = entry.metadata() else { continue };
                if meta.is_dir() {
                    walk(&path, acc);
                } else if meta.is_file() {
                    *acc += 1;
                }
            }
        }
    }
    let mut n = 0;
    walk(p, &mut n);
    n
}

#[cfg(test)]
mod tests {
    use super::*;

    fn seed_connection(parent: &Path, name: &str, api_base: &str, org_id: u64) {
        let folder = parent.join(name);
        std::fs::create_dir_all(&folder).unwrap();
        std::fs::write(
            folder.join("rdc.toml"),
            format!("[envs.main]\napi_base = \"{api_base}\"\norg_id = {org_id}\n"),
        )
        .unwrap();
    }

    #[test]
    fn scan_finds_rdc_projects_sorts_by_name() {
        let tmp = tempfile::tempdir().unwrap();
        seed_connection(tmp.path(), "zebra", "https://a/api/v1", 1);
        seed_connection(tmp.path(), "alpha", "https://b/api/v1", 2);
        std::fs::create_dir_all(tmp.path().join("not-a-project")).unwrap();

        let cs = scan(tmp.path());
        assert_eq!(cs.len(), 2);
        assert_eq!(cs[0].name(), "alpha");
        assert_eq!(cs[1].name(), "zebra");
        assert_eq!(cs[0].api_base, "https://b/api/v1");
        assert_eq!(cs[0].org_id, 2);
    }

    #[test]
    fn find_returns_named_connection() {
        let tmp = tempfile::tempdir().unwrap();
        seed_connection(tmp.path(), "acme", "https://x/api/v1", 7);
        let c = find(tmp.path(), "acme").unwrap();
        assert_eq!(c.api_base, "https://x/api/v1");
        assert_eq!(c.org_id, 7);
        assert_eq!(c.auth_kind, AuthKind::Token);
    }

    #[test]
    fn find_returns_none_for_missing() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(find(tmp.path(), "nope").is_none());
    }

    #[test]
    fn auth_kind_is_password_when_username_in_secrets() {
        let tmp = tempfile::tempdir().unwrap();
        seed_connection(tmp.path(), "p", "https://x/api/v1", 1);
        rdc::secrets::save_password_credentials(&tmp.path().join("p"), "main", "u", "pw").unwrap();
        let c = find(tmp.path(), "p").unwrap();
        assert_eq!(c.auth_kind, AuthKind::Password);
    }
}
