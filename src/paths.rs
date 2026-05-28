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

    /// `<root>/.rdc/state/<env>.lock` — advisory lock file (sibling of the
    /// JSON lockfile content). Empty file; existence is incidental. Used by
    /// `EnvLock` for cross-process write serialization.
    pub fn env_lock(&self) -> PathBuf {
        self.root
            .join(".rdc")
            .join("state")
            .join(format!("{}.lock", self.env))
    }

    /// `<root>/envs/<env>/`
    pub fn env_root(&self) -> PathBuf {
        self.root.join("envs").join(&self.env)
    }

    /// `<root>/.rdc/state/<env>.base/`. Mirrors the env tree one-to-one
    /// and stores the last-synced bytes of every tracked file (JSON +
    /// `.py` / `.js` / formula sidecars). Used by sync's 3-way merge
    /// to recover the merge base when local and remote both diverged.
    /// See `state::base_cache` for the read / write / GC helpers.
    pub fn base_cache_root(&self) -> PathBuf {
        self.root
            .join(".rdc")
            .join("state")
            .join(format!("{}.base", self.env))
    }

    /// `<root>/envs/<env>/organization.json`
    pub fn organization_file(&self) -> PathBuf {
        self.env_root().join("organization.json")
    }

    /// `<root>/envs/<env>/overlay.toml`
    pub fn overlay_file(&self) -> PathBuf {
        self.env_root().join("overlay.toml")
    }

    /// `<root>/.rdc/map/`
    pub fn mapping_dir(&self) -> PathBuf {
        self.root.join(".rdc").join("map")
    }

    /// `<root>/.rdc/map/<src>-to-<tgt>.toml`
    pub fn mapping_file(&self, src: &str, tgt: &str) -> PathBuf {
        self.mapping_dir().join(format!("{src}-to-{tgt}.toml"))
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

    /// `<root>/envs/<env>/engines/<engine_slug>/`. Mirrors the
    /// workspace-as-directory pattern: the engine's own JSON lives at
    /// `engine.json` inside this dir, alongside a `fields/` subdir for
    /// the engine fields it owns.
    pub fn engine_dir(&self, engine_slug: &str) -> PathBuf {
        self.engines_dir().join(engine_slug)
    }

    /// `<root>/envs/<env>/engines/<engine_slug>/fields/`. One file per
    /// engine field; each engine field belongs to exactly one engine.
    pub fn engine_fields_dir(&self, engine_slug: &str) -> PathBuf {
        self.engine_dir(engine_slug).join("fields")
    }

    /// Find which engine contains the given field by walking
    /// `engines/<e_slug>/fields/<field_slug>.json` on disk. Returns the
    /// engine slug of the first match, or `None` if the field file isn't
    /// found under any engine. Used by deploy's preview and create paths
    /// to resolve the engine context for a field (the field slug alone
    /// isn't enough — fields are nested under engines).
    pub fn engine_slug_for_field(&self, field_slug: &str) -> Option<String> {
        let dir = self.engines_dir();
        if !dir.exists() {
            return None;
        }
        for entry in std::fs::read_dir(&dir).ok()? {
            let Ok(entry) = entry else { continue };
            if !entry.file_type().ok()?.is_dir() {
                continue;
            }
            let e_slug = entry.file_name().to_string_lossy().into_owned();
            if self
                .engine_fields_dir(&e_slug)
                .join(format!("{field_slug}.json"))
                .exists()
            {
                return Some(e_slug);
            }
        }
        None
    }

    /// `<root>/envs/<env>/workflows/`
    pub fn workflows_dir(&self) -> PathBuf {
        self.env_root().join("workflows")
    }

    /// `<root>/envs/<env>/workflows/<workflow_slug>/`. Same dir-with-
    /// named-json pattern as workspaces and engines: the workflow's
    /// own JSON lives at `workflow.json` inside this dir, alongside a
    /// `steps/` subdir for the workflow steps it owns.
    pub fn workflow_dir(&self, workflow_slug: &str) -> PathBuf {
        self.workflows_dir().join(workflow_slug)
    }

    /// `<root>/envs/<env>/workflows/<workflow_slug>/steps/`. One file
    /// per workflow step; each step belongs to exactly one workflow.
    pub fn workflow_steps_dir(&self, workflow_slug: &str) -> PathBuf {
        self.workflow_dir(workflow_slug).join("steps")
    }

    /// `<root>/envs/<env>/workspaces/<ws_slug>/queues/<queue_slug>/email-templates/`.
    /// Email templates are queue-scoped in the live API; the snapshot mirrors
    /// that nesting.
    pub fn queue_email_templates_dir(&self, ws_slug: &str, queue_slug: &str) -> PathBuf {
        self.queue_dir(ws_slug, queue_slug).join("email-templates")
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

/// Returns true if this filename is a sync-generated shadow artifact for the
/// given env. Snapshot walkers use this to skip the conflict-skip shadow
/// (`<file>.<env>`) and the remote-delete marker (`<file>.<env>-deleted`).
///
/// Corner case: env names that are suffixes of each other (e.g. `dev` and
/// `dev-deleted`) would alias here — a project that defines both as real
/// envs would see this predicate misclassify a `<file>.dev-deleted` from
/// the `dev-deleted` env as a remote-delete marker for `dev`. `rdc init`'s
/// validator allows any `[A-Za-z0-9_-]+` env name, so this is technically
/// possible but never seen in practice.
pub fn is_shadow_artifact(name: &str, env: &str) -> bool {
    name.ends_with(&format!(".{env}")) || name.ends_with(&format!(".{env}-deleted"))
}

/// Compute the sibling shadow-file path for a local file under sync.
/// Returns `<local_path>.<env>` (e.g. `x.json.production`).
/// For local paths with no `file_name()` (rare; defensive), falls back to
/// `<parent>/shadow.<env>` — and to bare `shadow.<env>` if the parent is
/// also absent.
pub fn shadow_path_for(local_path: &Path, env: &str) -> PathBuf {
    let parent = local_path.parent().unwrap_or_else(|| Path::new(""));
    match local_path.file_name().and_then(|f| f.to_str()) {
        Some(name) => parent.join(format!("{name}.{env}")),
        None => parent.join(format!("shadow.{env}")),
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
    fn env_lock_path() {
        assert_eq!(p().env_lock(), Path::new("/proj/.rdc/state/dev.lock"));
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
    fn mapping_dir_path() {
        assert_eq!(p().mapping_dir(), Path::new("/proj/.rdc/map"));
    }

    #[test]
    fn mapping_file_path() {
        assert_eq!(
            p().mapping_file("test", "prod"),
            Path::new("/proj/.rdc/map/test-to-prod.toml")
        );
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
    fn engine_dir_path() {
        assert_eq!(p().engine_dir("invoice"), Path::new("/proj/envs/dev/engines/invoice"));
    }

    #[test]
    fn engine_fields_dir_path() {
        assert_eq!(
            p().engine_fields_dir("invoice"),
            Path::new("/proj/envs/dev/engines/invoice/fields")
        );
    }

    #[test]
    fn workflows_dir_path() {
        assert_eq!(p().workflows_dir(), Path::new("/proj/envs/dev/workflows"));
    }

    #[test]
    fn workflow_dir_path() {
        assert_eq!(
            p().workflow_dir("ap-flow"),
            Path::new("/proj/envs/dev/workflows/ap-flow")
        );
    }

    #[test]
    fn workflow_steps_dir_path() {
        assert_eq!(
            p().workflow_steps_dir("ap-flow"),
            Path::new("/proj/envs/dev/workflows/ap-flow/steps")
        );
    }

    #[test]
    fn queue_email_templates_dir_path() {
        assert_eq!(
            p().queue_email_templates_dir("invoices-ap", "cost-invoices"),
            Path::new("/proj/envs/dev/workspaces/invoices-ap/queues/cost-invoices/email-templates")
        );
    }

    #[test]
    fn mdh_dir_path() {
        assert_eq!(p().mdh_dir(), Path::new("/proj/envs/dev/mdh"));
    }

    #[test]
    fn dataset_dir_path() {
        assert_eq!(p().dataset_dir("vendors"), Path::new("/proj/envs/dev/mdh/vendors"));
    }

    #[test]
    fn is_shadow_artifact_matches_env_suffix() {
        assert!(is_shadow_artifact("queue.json.dev", "dev"));
        assert!(is_shadow_artifact("schema.json.production", "production"));
        assert!(is_shadow_artifact("123.py.dev", "dev"));
    }

    #[test]
    fn is_shadow_artifact_matches_deleted_marker() {
        assert!(is_shadow_artifact("hook.json.dev-deleted", "dev"));
        assert!(is_shadow_artifact("rule.json.production-deleted", "production"));
    }

    #[test]
    fn is_shadow_artifact_rejects_other_envs() {
        assert!(!is_shadow_artifact("queue.json.production", "dev"));
        assert!(!is_shadow_artifact("queue.json.production-deleted", "dev"));
    }

    #[test]
    fn is_shadow_artifact_rejects_plain_files() {
        assert!(!is_shadow_artifact("queue.json", "dev"));
        assert!(!is_shadow_artifact("hook.py", "dev"));
        assert!(!is_shadow_artifact("workspace.json", "production"));
    }

    #[test]
    fn shadow_path_for_json_file() {
        assert_eq!(
            shadow_path_for(Path::new("envs/test/labels/audit-hold.json"), "production"),
            PathBuf::from("envs/test/labels/audit-hold.json.production")
        );
    }

    #[test]
    fn shadow_path_for_py_file() {
        assert_eq!(
            shadow_path_for(Path::new("envs/test/hooks/x.py"), "dev"),
            PathBuf::from("envs/test/hooks/x.py.dev")
        );
    }

    #[test]
    fn shadow_path_for_path_with_no_filename_falls_back() {
        // `Path::new("/")` has no `file_name()` and its `parent()` is
        // `None`, so the fallback parent is `""` and the result is
        // `shadow.<env>` with no parent prefix.
        assert_eq!(
            shadow_path_for(Path::new("/"), "dev"),
            PathBuf::from("shadow.dev")
        );
    }

    #[test]
    fn engine_slug_for_field_finds_field_by_walking_engines() {
        let dir = tempfile::TempDir::new().unwrap();
        let paths = Paths::for_env(dir.path(), "env");
        let fields_dir = paths.engine_fields_dir("my-engine");
        std::fs::create_dir_all(&fields_dir).unwrap();
        std::fs::write(fields_dir.join("item-qty.json"), b"{}").unwrap();

        assert_eq!(
            paths.engine_slug_for_field("item-qty"),
            Some("my-engine".to_string()),
        );
        assert_eq!(paths.engine_slug_for_field("missing"), None);
    }

    #[test]
    fn engine_slug_for_field_returns_none_when_engines_dir_missing() {
        let dir = tempfile::TempDir::new().unwrap();
        let paths = Paths::for_env(dir.path(), "env");
        // engines/ does not exist
        assert_eq!(paths.engine_slug_for_field("anything"), None);
    }
}
