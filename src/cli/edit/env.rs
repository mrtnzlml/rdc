//! `rdc edit env rename`: rename an environment everywhere in the project,
//! offline. The desktop app's rename calls this too.
//!
//! Spec: docs/superpowers/specs/2026-09-25-edit-env-rename-design.md
//!
//! Every check runs and every new file text is computed before anything
//! moves, so a refusal or a parse error leaves the project untouched. Once
//! applying starts, a failure restores each rewritten file and moves each
//! path back. `rdc.toml` is written last: it is the record that the env now
//! has its new name.

use crate::cli::edit::ci::{self, CiChange};
use crate::cli::sync::lock::EnvLock;
use crate::config::{valid_env_name, ProjectConfig, INVALID_ENV_NAME_MSG};
use crate::mapping::GenericMapping;
use crate::paths::Paths;
use crate::secrets::{env_var_for, hook_secrets_path};
use crate::snapshot::writer::write_atomic;
use anyhow::{bail, Context, Result};
use std::path::{Path, PathBuf};
use std::time::Duration;

/// What a rename did, or would do under `--dry-run`.
#[derive(Debug, Default)]
pub struct RenameReport {
    pub old: String,
    pub new: String,
    pub dry_run: bool,
    /// `(from, to)`, relative to the project root, in move order.
    pub moved: Vec<(PathBuf, PathBuf)>,
    /// Relative to the project root, in write order.
    pub rewritten: Vec<PathBuf>,
    pub ci_changes: Vec<CiChange>,
    pub warnings: Vec<String>,
    /// Work outside the repository that rdc cannot do.
    pub follow_ups: Vec<String>,
}

struct Rewrite {
    path: PathBuf,
    original: String,
    new: String,
}

pub fn rename_env(root: &Path, old: &str, new: &str, dry_run: bool) -> Result<RenameReport> {
    let new = new.trim();
    if !valid_env_name(old) || !valid_env_name(new) {
        bail!(INVALID_ENV_NAME_MSG);
    }
    let cfg = ProjectConfig::load(&root.join("rdc.toml"))?;
    if !cfg.envs.contains_key(old) {
        bail!("This project has no \"{old}\" environment.");
    }
    if new == old {
        bail!("The environment is already called \"{old}\".");
    }
    if new.eq_ignore_ascii_case(old) {
        bail!(
            "\"{old}\" and \"{new}\" differ only in letter case, which some file systems treat as \
             the same path. Rename to a different name first, then to \"{new}\"."
        );
    }
    if cfg.envs.contains_key(new) {
        bail!("An environment named \"{new}\" already exists in this project.");
    }
    let var = env_var_for(new, "TOKEN");
    if let Some(clash) = cfg
        .envs
        .keys()
        .filter(|e| e.as_str() != old)
        .find(|e| env_var_for(e, "TOKEN") == var)
    {
        bail!(
            "\"{new}\" would share the credential variable {var} with env \"{clash}\" (rdc maps \
             every character except letters and digits to '_'). Pick a distinct name."
        );
    }
    let pairs = move_pairs(root, &cfg, old, new);
    for (_, to) in &pairs {
        if to.symlink_metadata().is_ok() {
            bail!(
                "{} already exists. Move or delete it, then retry the rename.",
                rel(root, to).display()
            );
        }
    }
    let moves: Vec<(PathBuf, PathBuf)> =
        pairs.into_iter().filter(|(from, _)| from.symlink_metadata().is_ok()).collect();

    let lock_path = Paths::for_env(root, old).env_lock();
    let lock_existed = lock_path.exists();
    let lock = if dry_run { None } else { Some(EnvLock::acquire(&lock_path, Duration::ZERO)?) };
    let result = plan_and_apply(root, &cfg, old, new, dry_run, &moves);
    drop(lock);
    // The old env's lock file has no env left to guard after a rename, and
    // one this run created should not outlive a failed run either.
    if !dry_run && (result.is_ok() || !lock_existed) {
        let _ = std::fs::remove_file(&lock_path);
    }
    result
}

fn plan_and_apply(
    root: &Path,
    cfg: &ProjectConfig,
    old: &str,
    new: &str,
    dry_run: bool,
    moves: &[(PathBuf, PathBuf)],
) -> Result<RenameReport> {
    let mut report = RenameReport { old: old.into(), new: new.into(), dry_run, ..Default::default() };
    let mut new_cfg = cfg.clone();
    if let Some(env_cfg) = new_cfg.envs.remove(old) {
        new_cfg.envs.insert(new.to_string(), env_cfg);
    }

    let mut rewrites: Vec<Rewrite> = Vec::new();
    let mapping_path = Paths::for_env(root, old).mapping_file();
    if mapping_path.exists() {
        let original = read(&mapping_path)?;
        let mut mapping = GenericMapping::load(&mapping_path)?;
        let before = toml::to_string_pretty(&mapping).context("serializing mapping")?;
        mapping.rename_env(old, new);
        let text = toml::to_string_pretty(&mapping).context("serializing mapping")?;
        // Re-serializing drops the file's comments, so only a file with a
        // row naming the env is rewritten; the commented stub `rdc init`
        // scaffolds keeps its bytes.
        if text != before {
            push_rewrite(&mut rewrites, mapping_path, original, text);
        }
    }
    let doc_regions = crate::cli::scaffold_docs::render_doc_regions(&new_cfg.envs);
    for doc in ["README.md", "CLAUDE.md"] {
        let path = root.join(doc);
        if !path.exists() {
            continue;
        }
        let original = read(&path)?;
        let spliced = crate::cli::regions::splice(&original, &doc_regions, crate::cli::regions::MARKDOWN)
            .with_context(|| format!("reading the rdc regions of {doc}"))?;
        if let Some(text) = spliced {
            push_rewrite(&mut rewrites, path, original, text);
        }
    }
    let ci_path = root.join(".gitlab-ci.yml");
    if ci_path.exists() {
        let original = read(&ci_path)?;
        let pipeline = ci::rename_in_pipeline(&original, old, new, &new_cfg.envs)
            .context("planning the .gitlab-ci.yml rename")?;
        report.ci_changes = pipeline.changes;
        report.warnings = pipeline.warnings;
        if let Some(text) = pipeline.text {
            push_rewrite(&mut rewrites, ci_path, original, text);
        }
        report.follow_ups = follow_ups(old, new);
    }
    let toml_path = root.join("rdc.toml");
    let original = read(&toml_path)?;
    let text = toml::to_string_pretty(&new_cfg).context("serializing project config")?;
    push_rewrite(&mut rewrites, toml_path, original, text);

    report.moved = moves.iter().map(|(a, b)| (rel(root, a), rel(root, b))).collect();
    report.rewritten = rewrites.iter().map(|w| rel(root, &w.path)).collect();
    if !dry_run {
        apply(moves, &rewrites)?;
    }
    Ok(report)
}

/// Move every path, then write every file; undo all of it on the first
/// failure.
fn apply(moves: &[(PathBuf, PathBuf)], rewrites: &[Rewrite]) -> Result<()> {
    let mut moved: Vec<&(PathBuf, PathBuf)> = Vec::new();
    let mut written: Vec<&Rewrite> = Vec::new();
    let Err(err) = apply_forward(moves, rewrites, &mut moved, &mut written) else {
        return Ok(());
    };
    let mut failures: Vec<String> = Vec::new();
    for w in written.iter().rev() {
        if let Err(e) = write_atomic(&w.path, w.original.as_bytes()) {
            failures.push(format!("restoring {}: {e:#}", w.path.display()));
        }
    }
    for (from, to) in moved.iter().rev().map(|m| (&m.0, &m.1)) {
        if let Err(e) = std::fs::rename(to, from) {
            failures.push(format!("moving {} back: {e}", to.display()));
        }
    }
    if failures.is_empty() {
        Err(err.context("the rename failed, and every change was undone"))
    } else {
        Err(err.context(format!("the rename failed, and undoing it also failed: {}", failures.join("; "))))
    }
}

/// The forward half of [`apply`]: records each completed step so a failure
/// can be undone exactly.
fn apply_forward<'a>(
    moves: &'a [(PathBuf, PathBuf)],
    rewrites: &'a [Rewrite],
    moved: &mut Vec<&'a (PathBuf, PathBuf)>,
    written: &mut Vec<&'a Rewrite>,
) -> Result<()> {
    for m in moves {
        if let Some(parent) = m.1.parent() {
            std::fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
        }
        std::fs::rename(&m.0, &m.1)
            .with_context(|| format!("moving {} to {}", m.0.display(), m.1.display()))?;
        moved.push(m);
    }
    for w in rewrites {
        write_atomic(&w.path, w.new.as_bytes()).with_context(|| format!("writing {}", w.path.display()))?;
        written.push(w);
    }
    Ok(())
}

/// Every path that carries the env's name, as `(old, new)`.
fn move_pairs(root: &Path, cfg: &ProjectConfig, old: &str, new: &str) -> Vec<(PathBuf, PathBuf)> {
    let op = Paths::for_env(root, old);
    let np = Paths::for_env(root, new);
    let conflicts = root.join(".rdc").join("conflicts");
    let mut pairs = vec![
        (op.env_root(), np.env_root()),
        (op.secrets_file(), np.secrets_file()),
        (hook_secrets_path(root, old), hook_secrets_path(root, new)),
        (op.lockfile(), np.lockfile()),
        (op.base_cache_root(), np.base_cache_root()),
        (conflicts.join(old), conflicts.join(new)),
    ];
    // Legacy per-pair mapping files name both envs in the file name
    // (`.rdc/map/<a>-to-<b>.toml`); `rdc migrate` still converts them once.
    let map = op.mapping_dir();
    for other in cfg.envs.keys().filter(|e| e.as_str() != old) {
        pairs.push((map.join(format!("{old}-to-{other}.toml")), map.join(format!("{new}-to-{other}.toml"))));
        pairs.push((map.join(format!("{other}-to-{old}.toml")), map.join(format!("{other}-to-{new}.toml"))));
    }
    pairs
}

fn follow_ups(old: &str, new: &str) -> Vec<String> {
    vec![
        format!(
            "rename the CI variable {} to {}",
            env_var_for(old, "TOKEN"),
            env_var_for(new, "TOKEN")
        ),
        format!(
            "if the env signs in with a password, also rename {} and {} to {} and {}",
            env_var_for(old, "USER"),
            env_var_for(old, "PASS"),
            env_var_for(new, "USER"),
            env_var_for(new, "PASS")
        ),
        format!("GitLab keeps environment \"{old}\"'s deploy history; \"{new}\" starts a new one"),
    ]
}

fn push_rewrite(rewrites: &mut Vec<Rewrite>, path: PathBuf, original: String, new: String) {
    if new != original {
        rewrites.push(Rewrite { path, original, new });
    }
}

fn read(path: &Path) -> Result<String> {
    std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))
}

fn rel(root: &Path, path: &Path) -> PathBuf {
    path.strip_prefix(root).unwrap_or(path).to_path_buf()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;
    use tempfile::TempDir;

    fn project(envs: &[&str]) -> TempDir {
        let dir = TempDir::new().unwrap();
        let mut toml = String::new();
        for (i, e) in envs.iter().enumerate() {
            toml.push_str(&format!(
                "[envs.{e}]\napi_base = \"https://{e}.example.test/api/v1\"\norg_id = {}\n\n",
                i + 1
            ));
        }
        std::fs::write(dir.path().join("rdc.toml"), toml).unwrap();
        dir
    }

    fn write(root: &Path, rel: &str, body: &str) {
        let p = root.join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, body).unwrap();
    }

    /// Every per-env path `dev` can have.
    fn seed_dev(root: &Path) {
        write(root, "envs/dev/queues/invoices.json", "{}");
        write(root, "envs/dev/overlay.toml", "");
        write(root, "secrets/dev.secrets.json", "{\"api_token\":\"t\"}");
        write(root, "secrets/dev.hook-secrets.json", "{}");
        write(root, ".rdc/state/dev.lock.json", "{}");
        write(root, ".rdc/state/dev.base/queues/invoices.json", "{}");
        write(root, ".rdc/conflicts/dev/queues/invoices.json", "{}");
        write(root, ".rdc/mapping.toml", "version = 2\n\n[[queues]]\ndev = \"invoices\"\nprod = \"invoices-prod\"\n");
    }

    /// Files only, path -> bytes. Empty directories do not count.
    fn tree(root: &Path) -> BTreeMap<PathBuf, Vec<u8>> {
        fn walk(root: &Path, dir: &Path, out: &mut BTreeMap<PathBuf, Vec<u8>>) {
            for entry in std::fs::read_dir(dir).unwrap().flatten() {
                let p = entry.path();
                if p.is_dir() {
                    walk(root, &p, out);
                } else {
                    out.insert(p.strip_prefix(root).unwrap().to_path_buf(), std::fs::read(&p).unwrap());
                }
            }
        }
        let mut out = BTreeMap::new();
        walk(root, root, &mut out);
        out
    }

    #[test]
    fn moves_every_per_env_path_and_rewrites_the_config() {
        let dir = project(&["dev", "prod"]);
        let root = dir.path();
        seed_dev(root);
        let r = rename_env(root, "dev", "sandbox", false).unwrap();
        for p in [
            "envs/sandbox/queues/invoices.json",
            "envs/sandbox/overlay.toml",
            "secrets/sandbox.secrets.json",
            "secrets/sandbox.hook-secrets.json",
            ".rdc/state/sandbox.lock.json",
            ".rdc/state/sandbox.base/queues/invoices.json",
            ".rdc/conflicts/sandbox/queues/invoices.json",
        ] {
            assert!(root.join(p).exists(), "{p} should exist");
        }
        for p in ["envs/dev", "secrets/dev.secrets.json", "secrets/dev.hook-secrets.json",
                  ".rdc/state/dev.lock.json", ".rdc/state/dev.base", ".rdc/conflicts/dev",
                  ".rdc/state/dev.lock"] {
            assert!(!root.join(p).exists(), "{p} should be gone");
        }
        let cfg = ProjectConfig::load(&root.join("rdc.toml")).unwrap();
        assert_eq!(cfg.envs.keys().collect::<Vec<_>>(), vec!["prod", "sandbox"]);
        assert_eq!(cfg.envs["sandbox"].api_base, "https://dev.example.test/api/v1");
        let mapping = std::fs::read_to_string(root.join(".rdc/mapping.toml")).unwrap();
        assert!(mapping.contains("sandbox = \"invoices\""), "{mapping}");
        assert!(!mapping.contains("dev = "), "{mapping}");
        assert_eq!(r.moved.len(), 6);
        assert_eq!(r.rewritten, vec![PathBuf::from(".rdc/mapping.toml"), PathBuf::from("rdc.toml")]);
        assert!(r.follow_ups.is_empty(), "no pipeline, no GitLab follow-ups");
    }

    #[test]
    fn a_never_synced_env_only_rewrites_rdc_toml() {
        let dir = project(&["dev", "prod"]);
        let r = rename_env(dir.path(), "dev", "sandbox", false).unwrap();
        assert!(r.moved.is_empty());
        assert_eq!(r.rewritten, vec![PathBuf::from("rdc.toml")]);
        let cfg = ProjectConfig::load(&dir.path().join("rdc.toml")).unwrap();
        assert!(cfg.envs.contains_key("sandbox") && !cfg.envs.contains_key("dev"));
    }

    #[test]
    fn refusals_leave_the_tree_untouched() {
        let cases: &[(&str, &str, &str, &str)] = &[
            // (old, new, extra file to seed, expected error fragment)
            ("../x", "sandbox", "", "letters, digits"),
            ("dev", "a/b", "", "letters, digits"),
            ("nope", "sandbox", "", "no \"nope\" environment"),
            ("dev", "prod", "", "already exists in this project"),
            ("dev", "dev", "", "already called"),
            ("dev", "Dev", "", "differ only in letter case"),
            ("dev", "dev_us", "", "would share the credential variable RDC_TOKEN_DEV_US"),
            ("dev", "sandbox", "envs/sandbox/stray.json", "envs/sandbox already exists"),
            ("dev", "sandbox", "secrets/sandbox.secrets.json", "secrets/sandbox.secrets.json already exists"),
            ("dev", "sandbox", "secrets/sandbox.hook-secrets.json", "secrets/sandbox.hook-secrets.json already exists"),
            ("dev", "sandbox", ".rdc/state/sandbox.lock.json", "already exists"),
            ("dev", "sandbox", ".rdc/state/sandbox.base/x.json", "already exists"),
            ("dev", "sandbox", ".rdc/conflicts/sandbox/x.json", "already exists"),
        ];
        for (old, new, extra, expected) in cases {
            let dir = project(&["dev", "dev-us", "prod"]);
            let root = dir.path();
            seed_dev(root);
            if !extra.is_empty() {
                write(root, extra, "{}");
            }
            let before = tree(root);
            let err = rename_env(root, old, new, false).unwrap_err();
            assert!(format!("{err:#}").contains(expected), "{old} -> {new}: {err:#}");
            assert_eq!(tree(root), before, "{old} -> {new} changed the tree");
        }
    }

    #[test]
    fn refuses_while_another_process_holds_the_lock() {
        let dir = project(&["dev", "prod"]);
        let root = dir.path();
        seed_dev(root);
        let _held = EnvLock::acquire(&Paths::for_env(root, "dev").env_lock(), Duration::from_secs(1)).unwrap();
        let before = tree(root);
        let err = rename_env(root, "dev", "sandbox", false).unwrap_err();
        assert!(format!("{err:#}").contains("holding the lock on env 'dev'"), "{err:#}");
        assert_eq!(tree(root), before);
    }

    #[test]
    fn a_failed_write_undoes_every_move_and_write() {
        let dir = project(&["dev", "prod"]);
        let root = dir.path();
        seed_dev(root);
        write(root, "README.md", "# Project\n<!-- >>> rdc:envs -->\nstale\n<!-- <<< rdc:envs -->\n");
        // `write_atomic` writes `README.md.tmp` first; a directory there makes
        // the README write fail after the moves and the mapping write.
        std::fs::create_dir_all(root.join("README.md.tmp")).unwrap();
        let before = tree(root);
        let err = rename_env(root, "dev", "sandbox", false).unwrap_err();
        assert!(format!("{err:#}").contains("every change was undone"), "{err:#}");
        assert_eq!(tree(root), before);
        assert!(!root.join("envs/sandbox").exists());
    }

    #[test]
    fn dry_run_writes_nothing_and_reports_the_plan() {
        let dir = project(&["dev", "prod"]);
        let root = dir.path();
        seed_dev(root);
        let before = tree(root);
        let r = rename_env(root, "dev", "sandbox", true).unwrap();
        assert_eq!(tree(root), before);
        assert!(!Paths::for_env(root, "dev").env_lock().exists());
        assert!(r.dry_run);
        assert_eq!(r.moved.len(), 6);
        assert_eq!(r.rewritten.len(), 2);
    }

    #[test]
    fn a_mapping_with_no_row_for_the_env_keeps_its_bytes() {
        let dir = project(&["dev", "prod"]);
        let root = dir.path();
        let stub = "# Slugs that differ between envs.\n#\n# [[queues]]\n# dev = \"ap\"\n";
        write(root, ".rdc/mapping.toml", stub);
        let r = rename_env(root, "dev", "sandbox", false).unwrap();
        assert_eq!(std::fs::read_to_string(root.join(".rdc/mapping.toml")).unwrap(), stub);
        assert!(!r.rewritten.contains(&PathBuf::from(".rdc/mapping.toml")));
    }

    #[test]
    fn a_leftover_job_for_the_new_name_refuses_before_writing() {
        let dir = project(&["dev", "prod"]);
        let root = dir.path();
        seed_dev(root);
        write(
            root,
            ".gitlab-ci.yml",
            "# >>> rdc:deploy-jobs\n\"deploy:sandbox\":\n  extends: .rdc-deploy\n# <<< rdc:deploy-jobs\n",
        );
        let before = tree(root);
        let err = rename_env(root, "dev", "sandbox", false).unwrap_err();
        assert!(format!("{err:#}").contains("already has a deploy:sandbox job"), "{err:#}");
        assert_eq!(tree(root), before);
    }

    #[test]
    fn renames_legacy_pair_mapping_files() {
        let dir = project(&["dev", "prod"]);
        let root = dir.path();
        write(root, ".rdc/map/dev-to-prod.toml", "");
        write(root, ".rdc/map/prod-to-dev.toml", "");
        rename_env(root, "dev", "sandbox", false).unwrap();
        assert!(root.join(".rdc/map/sandbox-to-prod.toml").exists());
        assert!(root.join(".rdc/map/prod-to-sandbox.toml").exists());
        assert!(!root.join(".rdc/map/dev-to-prod.toml").exists());
    }

    #[test]
    fn refreshes_doc_regions_and_the_pipeline() {
        let dir = project(&["dev", "prod", "test"]);
        let root = dir.path();
        let readme = "# Project\n\n<!-- >>> rdc:envs -->\nstale\n<!-- <<< rdc:envs -->\n\nHand-written.\n";
        write(root, "README.md", readme);
        write(root, "CLAUDE.md", "No markers here; dev is mentioned.\n");
        write(root, ".gitlab-ci.yml", crate::cli::init::GITLAB_CI_TEMPLATE);
        let r = rename_env(root, "dev", "sandbox", false).unwrap();

        let cfg = ProjectConfig::load(&root.join("rdc.toml")).unwrap();
        let expected = crate::cli::regions::splice(
            readme,
            &crate::cli::scaffold_docs::render_doc_regions(&cfg.envs),
            crate::cli::regions::MARKDOWN,
        )
        .unwrap()
        .unwrap();
        assert_eq!(std::fs::read_to_string(root.join("README.md")).unwrap(), expected);
        assert_eq!(
            std::fs::read_to_string(root.join("CLAUDE.md")).unwrap(),
            "No markers here; dev is mentioned.\n"
        );

        let ci = std::fs::read_to_string(root.join(".gitlab-ci.yml")).unwrap();
        assert!(ci.contains("\"deploy:sandbox\":"));
        assert!(!ci.contains("\"deploy:dev\":"));
        assert_eq!(ci.matches("extends: .rdc-deploy").count(), 3, "no draft appended");
        assert!(r.ci_changes.iter().any(|c| c.to_string() == "deploy:dev -> deploy:sandbox"));
        assert!(r.follow_ups[0].contains("RDC_TOKEN_DEV to RDC_TOKEN_SANDBOX"), "{:?}", r.follow_ups);
        assert!(!r.rewritten.contains(&PathBuf::from("CLAUDE.md")));
    }
}
