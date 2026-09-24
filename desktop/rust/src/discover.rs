//! Project discovery via directory scan. A Project is any folder under the
//! caller-supplied parent that looks like an rdc project: an `rdc.toml` with
//! at least one `[envs.<name>]` section. Each env is surfaced independently.
//! All state is derived from on-disk artifacts — there is no registry.

use serde::Deserialize;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone)]
pub(crate) struct EnvInfo {
    pub name: String,
    pub api_base: String,
    pub org_id: u64,
    pub auth_kind: AuthKindRaw,
    pub last_sync_unix: Option<i64>,
    pub file_count: u64,
}

#[derive(Debug, Clone)]
pub(crate) struct Project {
    pub folder: PathBuf,
    pub envs: Vec<EnvInfo>, // non-empty, sorted by name
}

impl Project {
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

pub(crate) fn scan(parent: &Path) -> Vec<Project> {
    let mut out = Vec::new();
    let Ok(rd) = std::fs::read_dir(parent) else {
        return out;
    };
    for entry in rd.flatten() {
        if let Some(p) = inspect(&entry.path()) {
            out.push(p);
        }
    }
    out.sort_by(|a, b| a.name().cmp(b.name()));
    out
}

pub(crate) fn find(parent: &Path, name: &str) -> Option<Project> {
    inspect(&parent.join(name))
}

pub(crate) fn inspect(folder: &Path) -> Option<Project> {
    if !folder.is_dir() {
        return None;
    }
    let content = std::fs::read_to_string(folder.join("rdc.toml")).ok()?;
    let parsed: RdcToml = toml::from_str(&content).ok()?;
    if parsed.envs.is_empty() {
        return None;
    }
    // BTreeMap iterates in sorted key order → envs come out sorted by name.
    let envs: Vec<EnvInfo> = parsed
        .envs
        .into_iter()
        .map(|(name, cfg)| {
            let secrets = rdc::secrets::read_secrets_file(folder, &name).unwrap_or_default();
            let auth_kind = if secrets.username.is_some() {
                AuthKindRaw::Password
            } else {
                AuthKindRaw::Token
            };
            let last_sync_unix = last_sync_unix(folder, &name);
            let file_count = count_files(&folder.join(format!("envs/{name}")));
            EnvInfo {
                name,
                api_base: cfg.api_base,
                org_id: cfg.org_id,
                auth_kind,
                last_sync_unix,
                file_count,
            }
        })
        .collect();
    Some(Project {
        folder: folder.to_path_buf(),
        envs,
    })
}

/// When the env's lockfile last changed, or `None` if it has never synced.
///
/// A lockfile that tracks no object has not been synced: `rdc migrate` creates
/// one to record where the target's objects came from before the target's
/// first sync. So the file existing is not enough. The mtime stays
/// approximate either way: a `git pull` or a migrate that changes the lockfile
/// moves it too.
fn last_sync_unix(folder: &Path, env: &str) -> Option<i64> {
    let path = folder.join(format!(".rdc/state/{env}.lock.json"));
    let lockfile = rdc::state::Lockfile::load(&path).ok()?;
    if lockfile.objects.values().all(|by_slug| by_slug.is_empty()) {
        return None;
    }
    std::fs::metadata(&path)
        .ok()
        .and_then(|m| m.modified().ok())
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .and_then(|d| i64::try_from(d.as_secs()).ok())
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

    fn seed_env(parent: &Path, name: &str, env: &str, api_base: &str, org_id: u64) {
        let folder = parent.join(name);
        std::fs::create_dir_all(&folder).unwrap();
        let existing = std::fs::read_to_string(folder.join("rdc.toml")).unwrap_or_default();
        let block = format!("[envs.{env}]\napi_base = \"{api_base}\"\norg_id = {org_id}\n");
        std::fs::write(folder.join("rdc.toml"), format!("{existing}{block}")).unwrap();
    }

    #[test]
    fn inspect_reads_every_env_sorted() {
        let tmp = tempfile::tempdir().unwrap();
        seed_env(tmp.path(), "acme", "prod", "https://p.test/api/v1", 2);
        seed_env(tmp.path(), "acme", "dev", "https://d.test/api/v1", 1);
        let p = inspect(&tmp.path().join("acme")).unwrap();
        assert_eq!(p.name(), "acme");
        let names: Vec<&str> = p.envs.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, vec!["dev", "prod"]); // sorted
        assert_eq!(p.envs[0].org_id, 1);
        assert_eq!(p.envs[1].api_base, "https://p.test/api/v1");
    }

    #[test]
    fn inspect_discovers_project_without_a_main_env() {
        let tmp = tempfile::tempdir().unwrap();
        seed_env(tmp.path(), "cli", "dev", "https://d.test/api/v1", 7);
        let p = inspect(&tmp.path().join("cli")).unwrap();
        assert_eq!(p.envs.len(), 1);
        assert_eq!(p.envs[0].name, "dev");
    }

    #[test]
    fn inspect_none_when_no_envs_or_no_toml() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join("empty")).unwrap();
        std::fs::write(tmp.path().join("empty/rdc.toml"), "").unwrap();
        assert!(inspect(&tmp.path().join("empty")).is_none());
        assert!(inspect(&tmp.path().join("missing")).is_none());
    }

    #[test]
    fn scan_sorts_projects_by_name() {
        let tmp = tempfile::tempdir().unwrap();
        seed_env(tmp.path(), "zebra", "main", "https://z.test/api/v1", 1);
        seed_env(tmp.path(), "alpha", "main", "https://a.test/api/v1", 2);
        std::fs::create_dir_all(tmp.path().join("not-a-project")).unwrap();
        let ps = scan(tmp.path());
        assert_eq!(ps.iter().map(|p| p.name()).collect::<Vec<_>>(), vec!["alpha", "zebra"]);
    }

    /// A lockfile that tracks nothing is the one `rdc migrate` writes before
    /// the env's first sync, so the env still reads as never synced.
    #[test]
    fn a_lockfile_tracking_no_object_is_not_a_sync() {
        let tmp = tempfile::tempdir().unwrap();
        seed_env(tmp.path(), "acme", "prod", "https://p.test/api/v1", 2);
        let state = tmp.path().join("acme/.rdc/state");
        std::fs::create_dir_all(&state).unwrap();
        let lock = state.join("prod.lock.json");

        std::fs::write(
            &lock,
            r#"{"version":3,"objects":{},"origins":{"queues":{"invoices":{"env":"dev","id":1}}}}"#,
        )
        .unwrap();
        let p = find(tmp.path(), "acme").unwrap();
        assert_eq!(p.envs[0].last_sync_unix, None);

        std::fs::write(
            &lock,
            r#"{"version":3,"objects":{"queues":{"invoices":{"id":501}}}}"#,
        )
        .unwrap();
        let p = find(tmp.path(), "acme").unwrap();
        assert!(p.envs[0].last_sync_unix.is_some());
    }

    #[test]
    fn auth_kind_is_per_env() {
        let tmp = tempfile::tempdir().unwrap();
        seed_env(tmp.path(), "acme", "dev", "https://d.test/api/v1", 1);
        seed_env(tmp.path(), "acme", "prod", "https://p.test/api/v1", 2);
        rdc::secrets::save_password_credentials(&tmp.path().join("acme"), "dev", "u", "pw").unwrap();
        let p = find(tmp.path(), "acme").unwrap();
        let dev = p.envs.iter().find(|e| e.name == "dev").unwrap();
        let prod = p.envs.iter().find(|e| e.name == "prod").unwrap();
        assert_eq!(dev.auth_kind, AuthKindRaw::Password);
        assert_eq!(prod.auth_kind, AuthKindRaw::Token);
    }
}
