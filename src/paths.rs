//! Canonical filesystem paths for an rdc project.
//!
//! All path computation in the codebase MUST go through this module so the
//! layout is documented in one place and refactors don't drift across call
//! sites.

use std::path::{Path, PathBuf};

/// Name of the per-env shadow-override directory (`envs/<env>/overlay/`),
/// sibling of `overlay.toml`. Files under it shadow code/formula sidecars
/// during `rdc migrate`.
pub(crate) const OVERLAY_DIR: &str = "overlay";

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

    /// `<root>/.rdc/conflicts/<env>/<relpath>` — where sync parks the
    /// remote side of an unresolved conflict (the non-interactive shadow)
    /// and the `<relpath>-deleted` remote-delete marker. Mirrors the env
    /// tree one-to-one, exactly like [`base_cache_root`](Self::base_cache_root):
    /// the env is encoded in the directory path, so the shadow keeps the
    /// source file's normal name (no `.<env>` filename suffix). The whole
    /// tree is gitignored (see `cli::init::write_gitignore`) so shadows
    /// stay out of the working tree and out of commits.
    ///
    /// `relpath` is `local_path` relative to [`env_root`](Self::env_root);
    /// a path that isn't under the env tree falls back to the full
    /// `local_path` (defensive — production callers always pass an
    /// env-tree path).
    pub fn conflict_shadow_path(&self, local_path: &Path) -> PathBuf {
        let relpath = local_path.strip_prefix(self.env_root()).unwrap_or(local_path);
        self.root
            .join(".rdc")
            .join("conflicts")
            .join(&self.env)
            .join(relpath)
    }

    /// `<root>/envs/<env>/organization.json`
    pub fn organization_file(&self) -> PathBuf {
        self.env_root().join("organization.json")
    }

    /// `<root>/envs/<env>/overlay.toml`
    pub fn overlay_file(&self) -> PathBuf {
        self.env_root().join("overlay.toml")
    }

    /// `<root>/envs/<env>/overlay/` — shadow-override directory. Files mirror
    /// the snapshot tree and replace code/formula sidecars during `migrate`.
    pub fn overlay_dir(&self) -> PathBuf {
        self.env_root().join(OVERLAY_DIR)
    }

    /// `<root>/.rdc/map/`
    pub fn mapping_dir(&self) -> PathBuf {
        self.root.join(".rdc").join("map")
    }

    /// `<root>/.rdc/mapping.toml` — the single, direction-free slug map.
    pub fn mapping_file(&self) -> PathBuf {
        self.root.join(".rdc").join("mapping.toml")
    }

    /// Legacy per-pair mapping files `<root>/.rdc/map/<a>-to-<b>.toml`, if any.
    /// Superseded by [`Self::mapping_file`]; enumerated only to migrate them once.
    pub fn legacy_mapping_files(&self) -> Vec<PathBuf> {
        let dir = self.mapping_dir();
        let Ok(entries) = std::fs::read_dir(&dir) else {
            return Vec::new();
        };
        let mut out: Vec<PathBuf> = entries
            .flatten()
            .map(|e| e.path())
            .filter(|p| {
                p.extension().and_then(|s| s.to_str()) == Some("toml")
                    && p.file_stem()
                        .and_then(|s| s.to_str())
                        .map(|s| s.contains("-to-"))
                        .unwrap_or(false)
            })
            .collect();
        out.sort();
        out
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

    /// `<root>/envs/<env>/saved-views/`. Flat, one file per view: a saved view
    /// is org-scoped, and its `queues_filter` is a 0..n list, so there is no
    /// single owning queue to nest under.
    pub fn saved_views_dir(&self) -> PathBuf {
        self.env_root().join("saved-views")
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

    /// `<root>/envs/<env>/mdh/<dataset_slug>/data.jsonl` — the row data of a
    /// dataset flagged `"data": "manual"`. Absent for every other dataset.
    pub fn dataset_data(&self, dataset_slug: &str) -> PathBuf {
        self.dataset_dir(dataset_slug)
            .join(crate::snapshot::mdh_data::DATA_FILE)
    }
}

/// Returns true if this filename is a LEGACY sibling shadow artifact for
/// the given env: the conflict-skip shadow (`<file>.<env>`) or the
/// remote-delete marker (`<file>.<env>-deleted`). Newer runs write both
/// under the gitignored `.rdc/conflicts/<env>/` tree (see
/// [`Paths::conflict_shadow_path`]) so they never appear inside `envs/`;
/// this predicate remains so snapshot walkers keep skipping any old-style
/// siblings still sitting in a project's env tree (backward compat).
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
    fn overlay_dir_path() {
        assert_eq!(p().overlay_dir(), Path::new("/proj/envs/dev/overlay"));
    }

    #[test]
    fn mapping_dir_path() {
        assert_eq!(p().mapping_dir(), Path::new("/proj/.rdc/map"));
    }

    #[test]
    fn mapping_file_path() {
        assert_eq!(
            p().mapping_file(),
            Path::new("/proj/.rdc/mapping.toml")
        );
    }

    #[test]
    fn legacy_mapping_files_lists_pair_files_sorted() {
        let dir = tempfile::TempDir::new().unwrap();
        let paths = Paths::for_env(dir.path(), "dev");
        std::fs::create_dir_all(paths.mapping_dir()).unwrap();
        std::fs::write(paths.mapping_dir().join("test-to-prod.toml"), b"").unwrap();
        std::fs::write(paths.mapping_dir().join("dev-to-test.toml"), b"").unwrap();
        // Not a legacy pair file — must be ignored.
        std::fs::write(paths.mapping_dir().join("notes.txt"), b"").unwrap();

        assert_eq!(
            paths.legacy_mapping_files(),
            vec![
                paths.mapping_dir().join("dev-to-test.toml"),
                paths.mapping_dir().join("test-to-prod.toml"),
            ]
        );
    }

    #[test]
    fn legacy_mapping_files_empty_when_dir_missing() {
        let dir = tempfile::TempDir::new().unwrap();
        let paths = Paths::for_env(dir.path(), "dev");
        assert!(paths.legacy_mapping_files().is_empty());
    }

    #[test]
    fn hooks_dir_path() {
        assert_eq!(p().hooks_dir(), Path::new("/proj/envs/dev/hooks"));
    }

    #[test]
    fn workspace_dir_path() {
        assert_eq!(p().workspace_dir("dev-us"), Path::new("/proj/envs/dev/workspaces/dev-us"));
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
    fn saved_views_dir_path() {
        assert_eq!(p().saved_views_dir(), Path::new("/proj/envs/dev/saved-views"));
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
    fn dataset_data_path() {
        assert_eq!(
            p().dataset_data("gl-codes"),
            Path::new("/proj/envs/dev/mdh/gl-codes/data.jsonl")
        );
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
    fn conflict_shadow_path_mirrors_env_tree_json_file() {
        // The shadow keeps the source file's name and mirrors the env-tree
        // relpath under `.rdc/conflicts/<env>/` — no `.<env>` suffix.
        let p = Paths::for_env("/proj", "dev");
        assert_eq!(
            p.conflict_shadow_path(Path::new("/proj/envs/dev/labels/audit-hold.json")),
            Path::new("/proj/.rdc/conflicts/dev/labels/audit-hold.json")
        );
    }

    #[test]
    fn conflict_shadow_path_mirrors_env_tree_py_sidecar() {
        let p = Paths::for_env("/proj", "dev");
        assert_eq!(
            p.conflict_shadow_path(Path::new("/proj/envs/dev/hooks/x.py")),
            Path::new("/proj/.rdc/conflicts/dev/hooks/x.py")
        );
    }

    #[test]
    fn conflict_shadow_path_falls_back_when_outside_env_tree() {
        // A path not under `env_root()` is used verbatim as the relpath
        // (defensive — production callers always pass an env-tree path).
        let p = Paths::for_env("/proj", "dev");
        assert_eq!(
            p.conflict_shadow_path(Path::new("stray.json")),
            Path::new("/proj/.rdc/conflicts/dev/stray.json")
        );
    }
}
