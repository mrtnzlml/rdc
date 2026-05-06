//! Canonical filesystem paths for an rdc project.
//!
//! All path computation in the codebase MUST go through this module so the
//! layout is documented in one place and refactors don't drift across call
//! sites.

use std::path::{Path, PathBuf};

/// Bundle of paths derived from a project root and an environment name.
#[derive(Debug, Clone)]
pub struct Paths {
    root: PathBuf,
    env: String,
}

impl Paths {
    /// Create a `Paths` for `<root>` and a specific environment.
    pub fn for_env(root: impl Into<PathBuf>, env: impl Into<String>) -> Self {
        Self { root: root.into(), env: env.into() }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn env(&self) -> &str {
        &self.env
    }

    /// `<root>/rdc.toml`
    pub fn project_config(&self) -> PathBuf {
        self.root.join("rdc.toml")
    }

    /// `<root>/secrets/<env>.secrets.json`
    pub fn secrets_file(&self) -> PathBuf {
        self.root.join("secrets").join(format!("{}.secrets.json", self.env))
    }

    /// `<root>/.rdc/state/<env>.lock.json`
    pub fn lockfile(&self) -> PathBuf {
        self.root
            .join(".rdc")
            .join("state")
            .join(format!("{}.lock.json", self.env))
    }

    /// `<root>/envs/<env>/`
    pub fn env_root(&self) -> PathBuf {
        self.root.join("envs").join(&self.env)
    }

    /// `<root>/envs/<env>/organization.json`
    pub fn organization_file(&self) -> PathBuf {
        self.env_root().join("organization.json")
    }

    /// `<root>/envs/<env>/hooks/`
    pub fn hooks_dir(&self) -> PathBuf {
        self.env_root().join("hooks")
    }

    /// `<root>/envs/<env>/workspaces/`
    pub fn workspaces_dir(&self) -> PathBuf {
        self.env_root().join("workspaces")
    }

    /// `<root>/envs/<env>/workspaces/<slug>/`
    pub fn workspace_dir(&self, slug: &str) -> PathBuf {
        self.workspaces_dir().join(slug)
    }

    /// `<root>/envs/<env>/workspaces/<ws_slug>/queues/`
    pub fn queues_dir(&self, ws_slug: &str) -> PathBuf {
        self.workspace_dir(ws_slug).join("queues")
    }

    /// `<root>/envs/<env>/workspaces/<ws_slug>/queues/<queue_slug>/`
    pub fn queue_dir(&self, ws_slug: &str, queue_slug: &str) -> PathBuf {
        self.queues_dir(ws_slug).join(queue_slug)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p() -> Paths {
        Paths::for_env("/proj", "dev")
    }

    #[test]
    fn project_config_path() {
        assert_eq!(p().project_config(), Path::new("/proj/rdc.toml"));
    }

    #[test]
    fn secrets_file_path() {
        assert_eq!(p().secrets_file(), Path::new("/proj/secrets/dev.secrets.json"));
    }

    #[test]
    fn lockfile_path() {
        assert_eq!(p().lockfile(), Path::new("/proj/.rdc/state/dev.lock.json"));
    }

    #[test]
    fn env_root_path() {
        assert_eq!(p().env_root(), Path::new("/proj/envs/dev"));
    }

    #[test]
    fn organization_file_path() {
        assert_eq!(p().organization_file(), Path::new("/proj/envs/dev/organization.json"));
    }

    #[test]
    fn hooks_dir_path() {
        assert_eq!(p().hooks_dir(), Path::new("/proj/envs/dev/hooks"));
    }

    #[test]
    fn workspace_dir_path() {
        assert_eq!(p().workspace_dir("dev-ap"), Path::new("/proj/envs/dev/workspaces/dev-ap"));
    }

    #[test]
    fn root_and_env_accessors() {
        let pp = Paths::for_env("/proj", "dev");
        assert_eq!(pp.root(), Path::new("/proj"));
        assert_eq!(pp.env(), "dev");
    }

    #[test]
    fn queues_dir_path() {
        assert_eq!(
            p().queues_dir("invoices-ap"),
            Path::new("/proj/envs/dev/workspaces/invoices-ap/queues")
        );
    }

    #[test]
    fn queue_dir_path() {
        assert_eq!(
            p().queue_dir("invoices-ap", "cost-invoices"),
            Path::new("/proj/envs/dev/workspaces/invoices-ap/queues/cost-invoices")
        );
    }
}
