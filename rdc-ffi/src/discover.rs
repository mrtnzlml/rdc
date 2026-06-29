//! Connection discovery via directory scan. A Connection is any folder
//! under the caller-supplied parent that looks like an rdc project: an
//! `rdc.toml` with an `[envs.main]` section. All state is derived from
//! on-disk artifacts — there is no registry.

use serde::Deserialize;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone)]
pub(crate) struct Connection {
    pub folder: PathBuf,
    pub api_base: String,
    pub org_id: u64,
    pub auth_kind: AuthKindRaw,
    pub last_sync_unix: Option<i64>,
    pub file_count: u64,
}

impl Connection {
    pub fn name(&self) -> &str {
        self.folder.file_name().and_then(|s| s.to_str()).unwrap_or("?")
    }
    /// Folder name — unique within the parent and stable across syncs.
    pub fn id(&self) -> &str {
        self.name()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AuthKindRaw {
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

pub(crate) fn scan(parent: &Path) -> Vec<Connection> {
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

pub(crate) fn find(parent: &Path, name: &str) -> Option<Connection> {
    inspect(&parent.join(name))
}

pub(crate) fn inspect(folder: &Path) -> Option<Connection> {
    if !folder.is_dir() {
        return None;
    }
    let content = std::fs::read_to_string(folder.join("rdc.toml")).ok()?;
    let parsed: RdcToml = toml::from_str(&content).ok()?;
    let env = parsed.envs.get("main")?;
    let secrets = rdc::secrets::read_secrets_file(folder, "main").unwrap_or_default();
    let auth_kind = if secrets.username.is_some() {
        AuthKindRaw::Password
    } else {
        AuthKindRaw::Token
    };
    let last_sync_unix = std::fs::metadata(folder.join(".rdc/state/main.lock.json"))
        .ok()
        .and_then(|m| m.modified().ok())
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .and_then(|d| i64::try_from(d.as_secs()).ok());
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

pub(crate) fn count_files(p: &Path) -> u64 {
    fn walk(p: &Path, acc: &mut u64) {
        if let Ok(rd) = std::fs::read_dir(p) {
            for entry in rd.flatten() {
                let Ok(meta) = entry.metadata() else { continue };
                if meta.is_dir() {
                    walk(&entry.path(), acc);
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
        seed_connection(tmp.path(), "zebra", "https://example.test/api/v1", 1);
        seed_connection(tmp.path(), "alpha", "https://other.test/api/v1", 2);
        std::fs::create_dir_all(tmp.path().join("not-a-project")).unwrap();

        let cs = scan(tmp.path());
        assert_eq!(cs.len(), 2);
        assert_eq!(cs[0].name(), "alpha");
        assert_eq!(cs[1].name(), "zebra");
        assert_eq!(cs[0].org_id, 2);
    }

    #[test]
    fn find_returns_named_connection() {
        let tmp = tempfile::tempdir().unwrap();
        seed_connection(tmp.path(), "acme", "https://example.test/api/v1", 7);
        let c = find(tmp.path(), "acme").unwrap();
        assert_eq!(c.api_base, "https://example.test/api/v1");
        assert_eq!(c.org_id, 7);
        assert_eq!(c.auth_kind, AuthKindRaw::Token);
    }

    #[test]
    fn find_returns_none_for_missing() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(find(tmp.path(), "nope").is_none());
    }

    #[test]
    fn auth_kind_is_password_when_username_in_secrets() {
        let tmp = tempfile::tempdir().unwrap();
        seed_connection(tmp.path(), "p", "https://example.test/api/v1", 1);
        rdc::secrets::save_password_credentials(&tmp.path().join("p"), "main", "u", "pw").unwrap();
        let c = find(tmp.path(), "p").unwrap();
        assert_eq!(c.auth_kind, AuthKindRaw::Password);
    }
}
