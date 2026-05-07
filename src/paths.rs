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

    /// `<root>/envs/<env>/overlay.toml`
    pub fn overlay_file(&self) -> PathBuf {
        self.env_root().join("overlay.toml")
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

    /// `<root>/envs/<env>/rules/`
    pub fn rules_dir(&self) -> PathBuf {
        self.env_root().join("rules")
    }

    /// `<root>/envs/<env>/labels/`
    pub fn labels_dir(&self) -> PathBuf {
        self.env_root().join("labels")
    }

    /// `<root>/envs/<env>/engines/`
    pub fn engines_dir(&self) -> PathBuf {
        self.env_root().join("engines")
    }

    /// `<root>/envs/<env>/engine-fields/`
    pub fn engine_fields_dir(&self) -> PathBuf {
        self.env_root().join("engine-fields")
    }

    /// `<root>/envs/<env>/workflows/`
    pub fn workflows_dir(&self) -> PathBuf {
        self.env_root().join("workflows")
    }

    /// `<root>/envs/<env>/workflow-steps/`
    pub fn workflow_steps_dir(&self) -> PathBuf {
        self.env_root().join("workflow-steps")
    }

    /// `<root>/envs/<env>/email-templates/`
    pub fn email_templates_dir(&self) -> PathBuf {
        self.env_root().join("email-templates")
    }

    /// `<root>/envs/<env>/mdh/`
    pub fn mdh_dir(&self) -> PathBuf {
        self.env_root().join("mdh")
    }

    /// `<root>/envs/<env>/mdh/<dataset_slug>/`
    pub fn dataset_dir(&self, dataset_slug: &str) -> PathBuf {
        self.mdh_dir().join(dataset_slug)
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
    fn overlay_file_path() {
        assert_eq!(p().overlay_file(), Path::new("/proj/envs/dev/overlay.toml"));
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

    #[test]
    fn rules_dir_path() {
        assert_eq!(p().rules_dir(), Path::new("/proj/envs/dev/rules"));
    }

    #[test]
    fn labels_dir_path() {
        assert_eq!(p().labels_dir(), Path::new("/proj/envs/dev/labels"));
    }

    #[test]
    fn engines_dir_path() {
        assert_eq!(p().engines_dir(), Path::new("/proj/envs/dev/engines"));
    }

    #[test]
    fn engine_fields_dir_path() {
        assert_eq!(p().engine_fields_dir(), Path::new("/proj/envs/dev/engine-fields"));
    }

    #[test]
    fn workflows_dir_path() {
        assert_eq!(p().workflows_dir(), Path::new("/proj/envs/dev/workflows"));
    }

    #[test]
    fn workflow_steps_dir_path() {
        assert_eq!(p().workflow_steps_dir(), Path::new("/proj/envs/dev/workflow-steps"));
    }

    #[test]
    fn email_templates_dir_path() {
        assert_eq!(p().email_templates_dir(), Path::new("/proj/envs/dev/email-templates"));
    }

    #[test]
    fn mdh_dir_path() {
        assert_eq!(p().mdh_dir(), Path::new("/proj/envs/dev/mdh"));
    }

    #[test]
    fn dataset_dir_path() {
        assert_eq!(p().dataset_dir("vendors"), Path::new("/proj/envs/dev/mdh/vendors"));
    }
}
