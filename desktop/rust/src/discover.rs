//! Project discovery via directory scan. A Project is any folder under the
//! caller-supplied parent that looks like an rdc project: an `rdc.toml` with
//! at least one `[envs.<name>]` section. Each env is surfaced independently.
//! All state is derived from on-disk artifacts — there is no registry.

use std::path::{Path, PathBuf};

/// One env of a project. The core type: the listing logic lives in
/// `rdc::cli::sync::embed` so every embedder shares it.
pub(crate) use rdc::cli::sync::embed::EnvInfo;
/// How an env authenticates (the core type under this module's old name).
pub(crate) use rdc::cli::sync::embed::AuthKind as AuthKindRaw;

#[derive(Debug, Clone)]
pub(crate) struct Project {
    pub folder: PathBuf,
    pub envs: Vec<EnvInfo>, // sorted by name; empty only when `error` is set
    /// Why `rdc.toml` failed to load. The project is still listed so the
    /// user sees what to fix instead of the folder silently disappearing.
    pub error: Option<String>,
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
    rdc::cli::sync::embed::inspect_project(folder).map(|info| Project {
        folder: folder.to_path_buf(),
        envs: info.envs,
        error: info.error,
    })
}

pub(crate) fn count_files(p: &Path) -> u64 {
    rdc::cli::sync::embed::count_files(p)
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
    fn inspect_reports_invalid_env_key_without_reading_outside_files() {
        // A committed `../..` key must not make the listing read another
        // project's secrets; the project is shown with the load error instead.
        let tmp = tempfile::tempdir().unwrap();
        let other = tmp.path().join("other");
        rdc::secrets::save_password_credentials(&other, "prod", "u", "pw").unwrap();
        seed_env(tmp.path(), "acme", "\"../../other/secrets/prod\"", "https://x.test/api/v1", 1);
        let p = inspect(&tmp.path().join("acme")).unwrap();
        assert!(p.envs.is_empty());
        let err = p.error.expect("load error is reported");
        assert!(err.contains("../../other/secrets/prod"), "got: {err}");
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
