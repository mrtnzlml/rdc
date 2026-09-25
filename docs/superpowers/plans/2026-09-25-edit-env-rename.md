# `rdc edit env rename` Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add `rdc edit env rename <old> <new> [--dry-run]`, an offline env rename in rdc core that the desktop app also uses.

**Architecture:** A pure pipeline-rewrite module (`src/cli/edit/ci.rs`) and a plan-then-apply rename (`src/cli/edit/env.rs`) sit under a new `edit` verb. The rename validates everything and computes every new file text before it moves anything, holds the old env's `EnvLock`, and rolls back every move and write on failure. The desktop bridge's `rename_env` becomes a thin wrapper that returns the rename's follow-ups to the app.

**Tech Stack:** Rust (clap 4, anyhow, toml, fs4 via `EnvLock`), flutter_rust_bridge 2.12.0, Flutter/Dart.

**Spec:** `docs/superpowers/specs/2026-09-25-edit-env-rename-design.md`

## Global Constraints

- Offline: no API call anywhere in this feature.
- Env-name rule: non-empty, ASCII letters, digits, `-`, `_` only. Invalid-name error text: `Environment name may only contain letters, digits, - and _.`
- Error texts the app and tests rely on, verbatim: `This project has no "<old>" environment.` and `An environment named "<new>" already exists in this project.`
- Never touch `.gitlab-ci.yml` text outside the `rdc:archive-envs` / `rdc:deploy-jobs` regions; inside `rdc:deploy-jobs` change only values equal to `<old>` and the `deploy:<old>` job key.
- A file with no rdc markers is never modified.
- No customer names or identifiers anywhere, including test fixtures and commit messages. Use `dev`, `prod`, `test`, `sandbox`, `dev-us`, `example.test`.
- rdc compiles slowly (user preference): write a task's tests and implementation, then build once. The separate "run it to see it fail" step is skipped.
- Never run repo-wide `cargo fmt`; the tree is not fmt-clean.
- Commit to local `main`; never push. Every commit message ends with `Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>`.
- CI gate, run from the repo root: `cargo test --locked`, `cargo clippy --all-targets --locked -- -D warnings`, `cargo doc --no-deps --document-private-items --locked`.

## Review Focus

1. **Env name that is a prefix of another** (`dev` next to `dev-us`): only exact `dev` values change; `dev-us` survives in CI and mapping. Pinned in Task 1 (`renames_exact_values_only`).
2. **Hand-edited unquoted value with a comment** (`RDC_SRC: dev   # promote from dev`): value rewritten, comment kept. Pinned in Task 1 (same test).
3. **Env that was never synced** (only an `rdc.toml` entry): rename succeeds and rewrites only `rdc.toml`. Pinned in Task 2 (`a_never_synced_env_only_rewrites_rdc_toml`).
4. **Case-only rename on a case-insensitive file system** (`dev` → `Dev` on macOS): a clear refusal, not a confusing "already exists". Pinned in Task 2 (`refusals_leave_the_tree_untouched`).
5. **Legacy per-pair mapping files** (`.rdc/map/dev-to-prod.toml`): renamed with the env, so a later `rdc migrate` conversion still finds them. Pinned in Task 2 (`renames_legacy_pair_mapping_files`).

Known and out of scope: `regions::splice` joins lines with `\n`, so a CRLF pipeline comes back LF — pre-existing `rdc init` behaviour.

---

### Task 1: Pipeline rewrite module

**Files:**
- Create: `src/cli/edit/mod.rs`
- Create: `src/cli/edit/ci.rs`
- Modify: `src/cli/regions.rs` (make `marker_name` `pub(crate)`)
- Modify: `src/cli/mod.rs` (add `pub mod edit;` after `pub mod doctor;`)

**Interfaces:**
- Consumes: `crate::cli::gitlab_ci::{render_regions_for_existing, REGION_ARCHIVE_ENVS, REGION_DEPLOY_JOBS}`, `crate::cli::regions::{splice, marker_name, YAML}`, `crate::config::EnvConfig`.
- Produces:
  - `pub struct CiChange { pub job: String, pub key: String, pub from: String, pub to: String }` with `Display`.
  - `pub struct PipelineRename { pub text: Option<String>, pub changes: Vec<CiChange>, pub warnings: Vec<String> }`
  - `pub fn rename_in_pipeline(existing: &str, old: &str, new: &str, new_envs: &BTreeMap<String, EnvConfig>) -> anyhow::Result<PipelineRename>`

- [ ] **Step 1: Expose `marker_name`**

In `src/cli/regions.rs` change `fn marker_name(line: &str, kind: &str, style: MarkerStyle) -> Option<String>` to `pub(crate) fn marker_name(...)`. It returns the full name, e.g. `Some("rdc:deploy-jobs")`.

- [ ] **Step 2: Create `src/cli/edit/mod.rs`**

```rust
//! `rdc edit`: local project maintenance. Every action under it works on
//! the project on disk and never contacts Rossum.

pub mod ci;
```

And in `src/cli/mod.rs`, add `pub mod edit;` directly after `pub mod doctor;`.

- [ ] **Step 3: Write `src/cli/edit/ci.rs` with its tests**

```rust
//! Renames an env inside an existing `.gitlab-ci.yml`, for `rdc edit env
//! rename`.
//!
//! Both generated regions name envs. `rdc:archive-envs` is re-rendered from
//! `rdc.toml` anyway, but `rdc:deploy-jobs` is the project's own text, and
//! `merge_deploy_jobs` decides which envs still need a draft by reading the
//! archive region on disk. So the old name is rewritten in both regions
//! first and the normal splice runs on the result: the renamed job counts as
//! already offered, and no duplicate draft is appended.

use crate::cli::gitlab_ci::{render_regions_for_existing, REGION_ARCHIVE_ENVS, REGION_DEPLOY_JOBS};
use crate::cli::regions::{self, YAML};
use crate::config::EnvConfig;
use anyhow::Result;
use std::collections::BTreeMap;

/// One value rewritten inside `rdc:deploy-jobs`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CiChange {
    /// The job the line belongs to, e.g. `deploy:test`.
    pub job: String,
    /// The YAML key whose value changed; empty for a job-key rename.
    pub key: String,
    pub from: String,
    pub to: String,
}

impl std::fmt::Display for CiChange {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.key.is_empty() {
            write!(f, "{} -> {}", self.from, self.to)
        } else {
            write!(f, "{} {} \"{}\" -> \"{}\"", self.job, self.key, self.from, self.to)
        }
    }
}

/// The outcome of renaming an env in a pipeline.
#[derive(Debug)]
pub struct PipelineRename {
    /// The new file text; `None` when the file has no rdc markers and is
    /// left alone.
    pub text: Option<String>,
    pub changes: Vec<CiChange>,
    pub warnings: Vec<String>,
}

/// Rename `old` to `new` in the pipeline text `existing`, then refresh both
/// regions for `new_envs` (the env set after the rename).
pub fn rename_in_pipeline(
    existing: &str,
    old: &str,
    new: &str,
    new_envs: &BTreeMap<String, EnvConfig>,
) -> Result<PipelineRename> {
    let (rewritten, changes, mut warnings) = rewrite_regions(existing, old, new);
    let regions = render_regions_for_existing(&rewritten, new_envs);
    let text = regions::splice(&rewritten, &regions, YAML)?;
    if text.is_none() {
        warnings.push(".gitlab-ci.yml has no rdc region markers, so rdc left it alone".to_string());
    }
    Ok(PipelineRename { text, changes, warnings })
}

fn rewrite_regions(existing: &str, old: &str, new: &str) -> (String, Vec<CiChange>, Vec<String>) {
    let old_job = format!("deploy:{old}");
    let new_job = format!("deploy:{new}");
    let mut out = String::with_capacity(existing.len());
    let mut changes = Vec::new();
    let mut warnings = Vec::new();
    let mut region: Option<String> = None;
    let mut job = String::new();
    for (idx, raw) in existing.split_inclusive('\n').enumerate() {
        let body = raw.trim_end_matches(['\n', '\r']);
        let eol = &raw[body.len()..];
        if let Some(name) = regions::marker_name(body, ">>>", YAML) {
            region = Some(name);
            out.push_str(raw);
            continue;
        }
        if regions::marker_name(body, "<<<", YAML).is_some() {
            region = None;
            out.push_str(raw);
            continue;
        }
        if let Some(key) = job_key(body) {
            job = key.to_string();
        }
        let in_deploy = region.as_deref() == Some(REGION_DEPLOY_JOBS);
        if !in_deploy && region.as_deref() != Some(REGION_ARCHIVE_ENVS) {
            if names_word(body, old) {
                warnings.push(format!(
                    ".gitlab-ci.yml line {}: still names \"{old}\" outside the rdc regions; rdc left it alone",
                    idx + 1
                ));
            }
            out.push_str(raw);
            continue;
        }
        if job_key(body) == Some(old_job.as_str()) {
            out.push_str(&body.replacen(&old_job, &new_job, 1));
            out.push_str(eol);
            if in_deploy {
                changes.push(CiChange {
                    job: old_job.clone(),
                    key: String::new(),
                    from: old_job.clone(),
                    to: new_job.clone(),
                });
            }
            job = new_job.clone();
            continue;
        }
        match rewrite_value(body, old, new) {
            Some((line, key)) => {
                out.push_str(&line);
                out.push_str(eol);
                // The archive region is re-rendered from rdc.toml right after,
                // so only the deploy jobs' changes are worth reporting.
                if in_deploy {
                    changes.push(CiChange { job: job.clone(), key, from: old.into(), to: new.into() });
                }
            }
            None => out.push_str(raw),
        }
    }
    (out, changes, warnings)
}

/// A top-level mapping key (`"deploy:dev":`, `stages:`), unquoted.
fn job_key(line: &str) -> Option<&str> {
    if line.starts_with(|c: char| c.is_whitespace() || c == '#' || c == '-') {
        return None;
    }
    let key = line.trim_end().strip_suffix(':')?;
    let key = key.strip_prefix('"').and_then(|k| k.strip_suffix('"')).unwrap_or(key);
    (!key.is_empty()).then_some(key)
}

/// If `line` is `key: value` (optionally a `- ` list item) whose scalar value
/// equals `old`, the line with `new` in its place — same quoting, same
/// trailing comment — and the key.
fn rewrite_value(line: &str, old: &str, new: &str) -> Option<(String, String)> {
    let indent_len = line.len() - line.trim_start().len();
    let (indent, rest) = line.split_at(indent_len);
    let (dash, rest) = match rest.strip_prefix("- ") {
        Some(r) => ("- ", r),
        None => ("", rest),
    };
    let colon = rest.find(": ")?;
    let key = &rest[..colon];
    if key.is_empty() || key.starts_with('#') || key.contains(char::is_whitespace) {
        return None;
    }
    let after = &rest[colon + 2..];
    let gap = &after[..after.len() - after.trim_start().len()];
    let value = &after[gap.len()..];
    let (scalar, quote, tail) = match value.chars().next() {
        Some(q @ ('"' | '\'')) => {
            let inner = &value[1..];
            let end = inner.find(q)?;
            (&inner[..end], Some(q), &inner[end + 1..])
        }
        _ => {
            let end = value.find(" #").unwrap_or(value.len());
            let scalar = value[..end].trim_end();
            (scalar, None, &value[scalar.len()..])
        }
    };
    if scalar != old {
        return None;
    }
    let replaced = match quote {
        Some(q) => format!("{q}{new}{q}"),
        None => new.to_string(),
    };
    Some((format!("{indent}{dash}{key}: {gap}{replaced}{tail}"), key.to_string()))
}

/// Whether `word` appears in `line` as a whole env name (not inside
/// `dev-us` or `RDC_TOKEN_DEV`).
fn names_word(line: &str, word: &str) -> bool {
    let is_name = |c: char| c.is_ascii_alphanumeric() || c == '-' || c == '_';
    line.match_indices(word).any(|(i, _)| {
        let before = line[..i].chars().next_back();
        let after = line[i + word.len()..].chars().next();
        !before.is_some_and(is_name) && !after.is_some_and(is_name)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const PIPELINE: &str = "\
stages: [archive, deploy]

archive:
  parallel:
    matrix:
      # >>> rdc:archive-envs  (generated)
      - RDC_ENV: \"dev\"
        RDC_VAR_SUFFIX: \"DEV\"
      - RDC_ENV: \"dev-us\"
        RDC_VAR_SUFFIX: \"DEV_US\"
      - RDC_ENV: \"prod\"
        RDC_VAR_SUFFIX: \"PROD\"
      # <<< rdc:archive-envs

# >>> rdc:deploy-jobs  (kept)
\"deploy:dev\":
  extends: .rdc-deploy
  resource_group: \"dev\"
  environment:
    name: \"dev\"
  variables:
    RDC_ENV: \"dev\"
    RDC_SRC: \"\"   # TODO: env to promote from

\"deploy:prod\":
  extends: .rdc-deploy
  variables:
    RDC_ENV: \"prod\"
    RDC_SRC: dev   # promote from dev

\"deploy:dev-us\":
  extends: .rdc-deploy
  variables:
    RDC_ENV: \"dev-us\"
    RDC_SRC: \"dev-us\"
# <<< rdc:deploy-jobs

notify:
  script: echo dev done
";

    fn envs(names: &[&str]) -> BTreeMap<String, EnvConfig> {
        names
            .iter()
            .map(|n| (n.to_string(), EnvConfig { api_base: "https://example.test/api/v1".into(), org_id: 1 }))
            .collect()
    }

    fn renamed() -> PipelineRename {
        rename_in_pipeline(PIPELINE, "dev", "sandbox", &envs(&["dev-us", "prod", "sandbox"])).unwrap()
    }

    #[test]
    fn renames_exact_values_only() {
        let r = renamed();
        let text = r.text.unwrap();
        assert!(text.contains("\"deploy:sandbox\":\n"));
        assert!(!text.contains("\"deploy:dev\":"));
        assert!(text.contains("  resource_group: \"sandbox\"\n"));
        assert!(text.contains("    name: \"sandbox\"\n"));
        assert!(text.contains("    RDC_ENV: \"sandbox\"\n"));
        // unquoted, hand-edited value: rewritten, comment kept as written
        assert!(text.contains("    RDC_SRC: sandbox   # promote from dev\n"));
        // a longer env name that starts with `dev` is left alone
        assert!(text.contains("\"deploy:dev-us\":\n"));
        assert!(text.contains("    RDC_ENV: \"dev-us\"\n    RDC_SRC: \"dev-us\"\n"));
        let changes: Vec<String> = r.changes.iter().map(ToString::to_string).collect();
        assert_eq!(
            changes,
            vec![
                "deploy:dev -> deploy:sandbox",
                "deploy:sandbox resource_group \"dev\" -> \"sandbox\"",
                "deploy:sandbox name \"dev\" -> \"sandbox\"",
                "deploy:sandbox RDC_ENV \"dev\" -> \"sandbox\"",
                "deploy:prod RDC_SRC \"dev\" -> \"sandbox\"",
            ]
        );
    }

    #[test]
    fn appends_no_draft_and_rerenders_the_archive() {
        let text = renamed().text.unwrap();
        assert_eq!(text.matches("extends: .rdc-deploy").count(), 3);
        assert!(text.contains("      - RDC_ENV: \"sandbox\"\n        RDC_VAR_SUFFIX: \"SANDBOX\"\n"));
        assert!(!text.contains("RDC_ENV: \"dev\"\n"));
        // re-rendered in rdc.toml's (sorted) order
        let prod = text.find("- RDC_ENV: \"prod\"").unwrap();
        let sandbox = text.find("- RDC_ENV: \"sandbox\"").unwrap();
        assert!(prod < sandbox);
    }

    #[test]
    fn leaves_the_rest_of_the_file_alone_and_warns() {
        let r = renamed();
        let text = r.text.unwrap();
        assert!(text.starts_with("stages: [archive, deploy]\n\narchive:\n"));
        assert!(text.ends_with("notify:\n  script: echo dev done\n"));
        assert_eq!(r.warnings.len(), 1, "{:?}", r.warnings);
        assert!(r.warnings[0].contains("still names \"dev\" outside the rdc regions"));
    }

    #[test]
    fn a_file_without_markers_is_left_alone() {
        let r = rename_in_pipeline("deploy:\n  script: echo dev\n", "dev", "sandbox", &envs(&["sandbox"])).unwrap();
        assert!(r.text.is_none());
        assert!(r.warnings.iter().any(|w| w.contains("no rdc region markers")));
    }

    #[test]
    fn broken_markers_are_an_error() {
        let broken = "# >>> rdc:deploy-jobs\n\"deploy:dev\":\n  extends: .rdc-deploy\n";
        assert!(rename_in_pipeline(broken, "dev", "sandbox", &envs(&["sandbox"])).is_err());
    }
}
```

- [ ] **Step 4: Build and run the tests**

Run: `cargo test --lib cli::edit::ci`
Expected: 5 passed. If `renames_exact_values_only` fails on the archive region's rendered indentation, compare against `render_archive_envs` in `src/cli/gitlab_ci.rs` and fix the fixture, not the renderer.

- [ ] **Step 5: Commit**

```bash
git add src/cli/edit src/cli/regions.rs src/cli/mod.rs
git commit -m "feat(edit): rewrite an env's name inside the pipeline regions

Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>"
```

---

### Task 2: Core env rename

**Files:**
- Create: `src/cli/edit/env.rs`
- Modify: `src/cli/edit/mod.rs` (add `pub mod env;`)
- Modify: `src/config/mod.rs` (add `valid_env_name`, `INVALID_ENV_NAME_MSG`)
- Modify: `src/cli/init.rs:443-449` (prompt validator calls `valid_env_name`)

**Interfaces:**
- Consumes: `ci::{rename_in_pipeline, CiChange}` (Task 1); `crate::cli::sync::lock::EnvLock::acquire(&Path, Duration) -> Result<EnvLock>`; `crate::paths::Paths`; `crate::secrets::{env_var_for, hook_secrets_path}`; `crate::mapping::GenericMapping`; `crate::snapshot::writer::write_atomic`; `crate::cli::scaffold_docs::render_doc_regions`; `crate::cli::regions::{splice, MARKDOWN}`.
- Produces:
  - `pub fn crate::config::valid_env_name(name: &str) -> bool`
  - `pub const crate::config::INVALID_ENV_NAME_MSG: &str`
  - `pub struct RenameReport { pub old: String, pub new: String, pub dry_run: bool, pub moved: Vec<(PathBuf, PathBuf)>, pub rewritten: Vec<PathBuf>, pub ci_changes: Vec<CiChange>, pub warnings: Vec<String>, pub follow_ups: Vec<String> }` (paths relative to the project root)
  - `pub fn rename_env(root: &Path, old: &str, new: &str, dry_run: bool) -> anyhow::Result<RenameReport>`

- [ ] **Step 1: Add the shared name rule to `src/config/mod.rs`**

Add above `impl EnvConfig`:

```rust
/// An env name may only contain ASCII letters, digits, `-` and `_`. The name
/// is interpolated into file paths (`envs/<env>/`, `secrets/<env>.*`), so
/// checking it is what keeps `..` or a path separator out of them.
pub fn valid_env_name(name: &str) -> bool {
    !name.is_empty() && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

/// The error for a name that fails [`valid_env_name`].
pub const INVALID_ENV_NAME_MSG: &str = "Environment name may only contain letters, digits, - and _.";
```

In `src/cli/init.rs`, in the prompt validator, replace

```rust
            if !trimmed
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
            {
```

with

```rust
            if !crate::config::valid_env_name(trimmed) {
```

The prompt's message text stays byte-identical (prompts are pinned).

- [ ] **Step 2: Write `src/cli/edit/env.rs`**

```rust
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
        mapping.rename_env(old, new);
        let text = toml::to_string_pretty(&mapping).context("serializing mapping")?;
        push_rewrite(&mut rewrites, mapping_path, original, text);
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
            .context("reading the rdc regions of .gitlab-ci.yml")?;
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
```

Add `pub mod env;` to `src/cli/edit/mod.rs` after `pub mod ci;`.

- [ ] **Step 3: Append the tests to `src/cli/edit/env.rs`**

```rust
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
```

- [ ] **Step 4: Build and run the tests**

Run: `cargo test --lib cli::edit`
Expected: all Task 1 and Task 2 tests pass. `extends: .rdc-deploy` also appears in the template's static half if a hidden job uses it — if the count assertion fails, count it in `GITLAB_CI_TEMPLATE` first and assert that number instead of 3.

Run: `cargo test --lib cli::init`
Expected: pass (prompt validator unchanged in behaviour).

- [ ] **Step 5: Commit**

```bash
git add src/cli/edit src/config/mod.rs src/cli/init.rs
git commit -m "feat(edit): rename an env everywhere in the project, with rollback

Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>"
```

---

### Task 3: `rdc edit` verb, output and docs

**Files:**
- Modify: `src/cli/edit/mod.rs` (clap types, `run`, report printing)
- Modify: `src/cli/mod.rs` (`Command::Edit` variant after `Doctor`, dispatch arm)
- Modify: `tests/cli_misc.rs` (prefix test gains `("e", "edit")`)
- Create: `tests/cli_edit.rs`
- Modify: `README.md` (Commands table row + `## \`rdc edit\`` section before `## Commands`)

**Interfaces:**
- Consumes: `env::{rename_env, RenameReport}` (Task 2); `crate::log::{Action, Log}`; `crate::cli::resolve::detect_color_mode()`; `super::env_name_candidates` (private fn in `src/cli/mod.rs`, visible to child modules).
- Produces: `pub enum EditCommand`, `pub enum EnvCommand`, `pub fn run(command: EditCommand) -> anyhow::Result<()>`.

- [ ] **Step 1: Add clap types and `run` to `src/cli/edit/mod.rs`**

Replace the file with:

```rust
//! `rdc edit`: local project maintenance. Every action under it works on
//! the project on disk and never contacts Rossum.

pub mod ci;
pub mod env;

use crate::log::{Action, Log};
use anyhow::Context;
use clap::Subcommand;
use clap_complete::ArgValueCandidates;

#[derive(Debug, Subcommand)]
pub enum EditCommand {
    /// Maintain the project's environments.
    Env {
        #[command(subcommand)]
        command: EnvCommand,
    },
}

#[derive(Debug, Subcommand)]
pub enum EnvCommand {
    /// Rename an environment everywhere in the project.
    #[command(long_about = RENAME_LONG_ABOUT)]
    Rename {
        /// The env's current name, as in `rdc.toml`.
        #[arg(add = ArgValueCandidates::new(super::env_name_candidates))]
        old: String,
        /// The new name. Letters, digits, `-` and `_` only.
        new: String,
        /// Print what the rename would do, and write nothing.
        #[arg(long = "dry-run")]
        dry_run: bool,
    },
}

const RENAME_LONG_ABOUT: &str = r#"Rename an environment everywhere in the project. Rossum is not contacted: the org, its objects and the token stay as they are.

Moves envs/<old>/, secrets/<old>.secrets.json, secrets/<old>.hook-secrets.json and the env's state under .rdc/. Renames the env in rdc.toml and .rdc/mapping.toml. Refreshes the rdc regions of README.md, CLAUDE.md and .gitlab-ci.yml. In the pipeline's deploy jobs it changes only values equal to <old> and the deploy:<old> job key.

It refuses before writing anything when <new> is taken, when a target path already exists, or when another rdc process holds <old>'s lock. If a step fails partway, it undoes every change.

The GitLab CI variables named after the env (RDC_TOKEN_<ENV>) cannot be renamed from here. The command prints what to rename."#;

pub fn run(command: EditCommand) -> anyhow::Result<()> {
    match command {
        EditCommand::Env { command: EnvCommand::Rename { old, new, dry_run } } => {
            let cwd = std::env::current_dir().context("getting current directory")?;
            let report = env::rename_env(&cwd, &old, &new, dry_run)?;
            print_report(&Log::new(crate::cli::resolve::detect_color_mode()), &report);
            Ok(())
        }
    }
}

fn print_report(log: &Log, r: &env::RenameReport) {
    let (action, verb) = if r.dry_run { (Action::Plan, "would rename") } else { (Action::Done, "renamed") };
    log.event(action, &format!("{verb} env {} -> {}", r.old, r.new));
    for (from, to) in &r.moved {
        log.row(&format!("         moved    {} -> {}", from.display(), to.display()));
    }
    for path in &r.rewritten {
        log.row(&format!("         rewrote  {}", path.display()));
    }
    for change in &r.ci_changes {
        log.row(&format!("         pipeline {change}"));
    }
    for warning in &r.warnings {
        log.event(Action::Warn, warning);
    }
    if !r.follow_ups.is_empty() {
        let lines: Vec<String> = r.follow_ups.iter().map(|f| format!("  {f}")).collect();
        log.block(&format!("still to do in GitLab:\n{}", lines.join("\n")));
    }
}
```

- [ ] **Step 2: Register the verb in `src/cli/mod.rs`**

In `enum Command`, between `Doctor { .. }` and `Upgrade { .. }`:

```rust
    /// Maintain the local project: rename an environment. Offline.
    Edit {
        #[command(subcommand)]
        command: crate::cli::edit::EditCommand,
    },
```

In `run`'s `match cli.command`, after the `Doctor` arm:

```rust
        Some(Command::Edit { command }) => crate::cli::edit::run(command),
```

- [ ] **Step 3: Extend the prefix test in `tests/cli_misc.rs`**

In `an_unambiguous_prefix_resolves_to_its_verb`, add `("e", "edit"),` after `("do", "doctor"),`.

- [ ] **Step 4: Write `tests/cli_edit.rs`**

```rust
//! End-to-end `rdc edit env rename` on a project scaffolded by `rdc init`.

use assert_cmd::Command;
use tempfile::TempDir;

fn scaffolded() -> TempDir {
    let dir = TempDir::new().unwrap();
    std::fs::write(
        dir.path().join("rdc.toml"),
        "[envs.dev]\napi_base = \"https://dev.example.test/api/v1\"\norg_id = 1\n\n\
         [envs.prod]\napi_base = \"https://prod.example.test/api/v1\"\norg_id = 2\n",
    )
    .unwrap();
    Command::cargo_bin("rdc").unwrap().current_dir(dir.path()).arg("init").assert().success();
    assert!(dir.path().join(".gitlab-ci.yml").exists(), "init should scaffold the pipeline");
    std::fs::create_dir_all(dir.path().join("envs/dev")).unwrap();
    std::fs::write(dir.path().join("envs/dev/organization.json"), "{}").unwrap();
    dir
}

fn rdc(dir: &TempDir, args: &[&str]) -> (bool, String) {
    let out = Command::cargo_bin("rdc").unwrap().current_dir(dir.path()).args(args).output().unwrap();
    let text = format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
    (out.status.success(), text)
}

#[test]
fn renames_the_env_and_reports_it() {
    let dir = scaffolded();
    let (ok, out) = rdc(&dir, &["edit", "env", "rename", "dev", "sandbox"]);
    assert!(ok, "{out}");
    assert!(out.contains("renamed env dev -> sandbox"), "{out}");
    assert!(out.contains("moved    envs/dev -> envs/sandbox"), "{out}");
    assert!(out.contains("rewrote  rdc.toml"), "{out}");
    assert!(out.contains("still to do in GitLab:"), "{out}");
    assert!(out.contains("RDC_TOKEN_DEV to RDC_TOKEN_SANDBOX"), "{out}");
    assert!(dir.path().join("envs/sandbox/organization.json").exists());
    let toml = std::fs::read_to_string(dir.path().join("rdc.toml")).unwrap();
    assert!(toml.contains("[envs.sandbox]") && !toml.contains("[envs.dev]"), "{toml}");

    // init afterwards has nothing left to fix: no new draft for `sandbox`
    let ci_before = std::fs::read_to_string(dir.path().join(".gitlab-ci.yml")).unwrap();
    let (ok, out) = rdc(&dir, &["init"]);
    assert!(ok, "{out}");
    assert_eq!(std::fs::read_to_string(dir.path().join(".gitlab-ci.yml")).unwrap(), ci_before);
}

#[test]
fn dry_run_writes_nothing() {
    let dir = scaffolded();
    let toml_before = std::fs::read_to_string(dir.path().join("rdc.toml")).unwrap();
    let (ok, out) = rdc(&dir, &["edit", "env", "rename", "dev", "sandbox", "--dry-run"]);
    assert!(ok, "{out}");
    assert!(out.contains("would rename env dev -> sandbox"), "{out}");
    assert!(dir.path().join("envs/dev/organization.json").exists());
    assert_eq!(std::fs::read_to_string(dir.path().join("rdc.toml")).unwrap(), toml_before);
}

#[test]
fn an_unknown_env_fails() {
    let dir = scaffolded();
    let (ok, out) = rdc(&dir, &["edit", "env", "rename", "nope", "sandbox"]);
    assert!(!ok);
    assert!(out.contains("This project has no \"nope\" environment."), "{out}");
}
```

- [ ] **Step 5: Document the verb in `README.md`**

Insert before `## Commands`:

~~~markdown
## `rdc edit`

Local project maintenance. Nothing under `rdc edit` contacts Rossum.

### Rename an environment

```sh
rdc edit env rename dev sandbox --dry-run   # show the plan
rdc edit env rename dev sandbox
```

It moves `envs/dev/`, both secrets files and the env's state under `.rdc/`. It
renames the env in `rdc.toml` and `.rdc/mapping.toml`, and refreshes the rdc
regions of `README.md`, `CLAUDE.md` and `.gitlab-ci.yml`. A finished deploy job
keeps its settings under the new name. It refuses while a sync runs on the env,
and undoes everything if a step fails.

Rename the GitLab CI variables named after the env (`RDC_TOKEN_DEV` →
`RDC_TOKEN_SANDBOX`) yourself; the command lists them.
~~~

In the `## Commands` table, add after the `rdc doctor <env>` row:

```markdown
| `rdc edit env rename <old> <new>` | Rename an env everywhere in the project, offline: files, state, `rdc.toml`, mapping, generated doc and pipeline regions. |
```

- [ ] **Step 6: Build and run**

Run: `cargo test --test cli_edit --test cli_misc --test command_references`
Expected: all pass. `every_arg_and_subcommand_has_help` covers the new subcommands; if it fails, the missing doc comment it names goes on that variant or field.

- [ ] **Step 7: Commit**

```bash
git add src/cli/edit/mod.rs src/cli/mod.rs tests/cli_misc.rs tests/cli_edit.rs README.md
git commit -m "feat(edit): add the \`rdc edit env rename\` command

Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>"
```

---

### Task 4: Desktop bridge uses the core rename

**Files:**
- Modify: `desktop/rust/src/api/rdc.rs` (`rename_env` → wrapper; drop local `valid_env_name` / `INVALID_ENV_NAME_MSG`; tests)
- Regenerate: FRB outputs (`desktop/rust/src/frb_generated.rs`, `desktop/lib/src/rust/**`)

**Interfaces:**
- Consumes: `rdc::cli::edit::env::rename_env(&Path, &str, &str, bool) -> Result<RenameReport>`; `rdc::config::{valid_env_name, INVALID_ENV_NAME_MSG}`.
- Produces (Dart via FRB): `Future<RenameEnvResult> renameEnv({required String folder, required String old, required String new_})` where `RenameEnvResult { ProjectSummary project; List<String> followUps; List<String> warnings; }`.

- [ ] **Step 1: Replace the local name rule**

Delete `fn valid_env_name` and `const INVALID_ENV_NAME_MSG` from `desktop/rust/src/api/rdc.rs` and add `use rdc::config::{valid_env_name, INVALID_ENV_NAME_MSG};` to its imports. Call sites keep their names.

- [ ] **Step 2: Replace `rename_env` and its doc comment**

```rust
/// What the app shows after a rename.
#[derive(Debug, Clone)]
pub struct RenameEnvResult {
    pub project: ProjectSummary,
    /// GitLab work rdc cannot do (CI variables, environment history).
    pub follow_ups: Vec<String>,
    pub warnings: Vec<String>,
}

/// Rename an environment `old` → `new`, offline. The work is
/// `rdc edit env rename`'s (`rdc::cli::edit::env::rename_env`), so the app
/// and the CLI rename the same paths, refuse the same cases and roll back
/// the same way.
pub fn rename_env(folder: String, old: String, new: String) -> Result<RenameEnvResult> {
    let folder = PathBuf::from(&folder);
    let summary = |f: &Path| {
        discover::inspect(f)
            .as_ref()
            .map(ProjectSummary::from)
            .ok_or_else(|| anyhow!("Project not found after rename_env"))
    };
    // The edit dialog submits the env name even when it is unchanged.
    if valid_env_name(&old) && new.trim() == old {
        return Ok(RenameEnvResult { project: summary(&folder)?, follow_ups: vec![], warnings: vec![] });
    }
    let report = rdc::cli::edit::env::rename_env(&folder, &old, &new, false).map_err(|e| anyhow!("{e:#}"))?;
    Ok(RenameEnvResult { project: summary(&folder)?, follow_ups: report.follow_ups, warnings: report.warnings })
}
```

- [ ] **Step 3: Update the bridge tests**

- `rename_env_moves_files_toml_and_mapping`: bind `let p = rename_env(...).unwrap().project;`. Keep its hook-secrets assertions.
- Delete `rename_env_rollback_is_symmetric_on_mapping_load_failure`; core's `a_failed_write_undoes_every_move_and_write` covers rollback now.
- `rename_env_rejects_collision_and_invalid` and `rename_env_rejects_invalid_old_name` stay as they are.
- Add:

```rust
    #[test]
    fn rename_env_returns_the_gitlab_follow_ups() {
        let tmp = tempfile::tempdir().unwrap();
        let folder = tmp.path().join("acme");
        std::fs::create_dir_all(&folder).unwrap();
        std::fs::write(folder.join("rdc.toml"),
            "[envs.dev]\napi_base = \"https://d.test/api/v1\"\norg_id = 1\n\
             [envs.prod]\napi_base = \"https://p.test/api/v1\"\norg_id = 2\n").unwrap();
        std::fs::write(folder.join(".gitlab-ci.yml"),
            "# >>> rdc:archive-envs\n- RDC_ENV: \"dev\"\n  RDC_VAR_SUFFIX: \"DEV\"\n# <<< rdc:archive-envs\n\
             # >>> rdc:deploy-jobs\n# <<< rdc:deploy-jobs\n").unwrap();
        let r = rename_env(folder.display().to_string(), "dev".into(), "sandbox".into()).unwrap();
        assert!(r.follow_ups[0].contains("RDC_TOKEN_SANDBOX"), "{:?}", r.follow_ups);
        assert!(r.project.envs.iter().any(|e| e.name == "sandbox"));
    }

    #[test]
    fn rename_env_to_the_same_name_is_a_no_op() {
        let tmp = tempfile::tempdir().unwrap();
        let folder = seed_project(tmp.path(), "acme");
        let r = rename_env(folder.display().to_string(), "main".into(), "main".into()).unwrap();
        assert!(r.follow_ups.is_empty());
        assert_eq!(r.project.envs[0].name, "main");
    }
```

- [ ] **Step 4: Regenerate the bindings**

Run: `cd desktop && flutter_rust_bridge_codegen generate`
Expected: `desktop/rust/src/frb_generated.rs` and `desktop/lib/src/rust/api/rdc.dart` (plus its freezed part if used) change; `RenameEnvResult` appears in the Dart file.

- [ ] **Step 5: Run the bridge tests**

Run: `cd desktop/rust && cargo test`
Expected: all pass.

- [ ] **Step 6: Do not commit yet**

`renameEnv` now returns `RenameEnvResult`, so the Dart side does not analyze until Task 5 adapts its callers. Task 5 commits both, keeping every commit green.

---

### Task 5: App shows the rename's follow-ups

**Files:**
- Modify: `desktop/lib/src/app_state.dart` (`editEnvEntry` returns notes)
- Modify: `desktop/lib/src/dialogs.dart` (`renameNotesMessage`, snackbar in `_EditConnectionDialogState._submit`)
- Modify: `desktop/test/env_naming_test.dart` (helper test)
- Modify: `desktop/integration_test/bridge_test.dart` (`.project` on the result)

**Interfaces:**
- Consumes: `renameEnv(...) -> Future<RenameEnvResult>` (Task 4).
- Produces: `Future<List<String>> AppState.editEnvEntry(...)`; top-level `String? renameNotesMessage(List<String> notes)` in `dialogs.dart`.

- [ ] **Step 1: `editEnvEntry` returns the notes**

In `desktop/lib/src/app_state.dart`, change the signature to `Future<List<String>> editEnvEntry(...)`, and:

```dart
    var targetEnv = env.name;
    var notes = const <String>[];
    final renamed = newEnvName != null && newEnvName != env.name;
    if (renamed) {
      if (!canRenameEnv(item.summary.folder, env.name)) {
        throw Exception("Can't rename while this environment is syncing.");
      }
      final r = await renameEnv(folder: item.summary.folder, old: env.name, new_: newEnvName);
      notes = [...r.warnings, ...r.followUps];
      targetEnv = newEnvName;
    }
```

and `return notes;` after `selectEnv(updated.folder, targetEnv);`. Add one line to its doc comment: `Returns the rename's warnings and GitLab follow-ups, empty when nothing was renamed.`

- [ ] **Step 2: Snackbar in the edit dialog**

In `desktop/lib/src/dialogs.dart`, add a top-level function:

```dart
/// The snackbar text after a rename, or null when there is nothing to say.
String? renameNotesMessage(List<String> notes) =>
    notes.isEmpty ? null : 'Renamed. Still to do:\n${notes.map((n) => '• $n').join('\n')}';
```

In `_EditConnectionDialogState._submit`, replace

```dart
      await widget.state.editEnvEntry(
```

with `final notes = await widget.state.editEnvEntry(`, and replace `if (mounted) Navigator.of(context).pop(true);` (the one in this dialog, line ~476) with:

```dart
      if (!mounted) return;
      final messenger = ScaffoldMessenger.of(context);
      Navigator.of(context).pop(true);
      final message = renameNotesMessage(notes);
      if (message != null) {
        messenger.showSnackBar(SnackBar(
          content: SelectableText(message),
          duration: const Duration(seconds: 12),
        ));
      }
```

The dialog keeps popping `bool`, so `showDialog<bool>` callers and the cancel button's `pop(false)` are unaffected.

- [ ] **Step 3: Tests**

Append to `desktop/test/env_naming_test.dart` (add `import 'package:desktop/src/dialogs.dart';` if absent):

```dart
  test('renameNotesMessage is null without notes and lists them otherwise', () {
    expect(renameNotesMessage(const []), isNull);
    expect(
      renameNotesMessage(const ['rename the CI variable RDC_TOKEN_DEV to RDC_TOKEN_SANDBOX']),
      'Renamed. Still to do:\n• rename the CI variable RDC_TOKEN_DEV to RDC_TOKEN_SANDBOX',
    );
  });
```

In `desktop/integration_test/bridge_test.dart`, the rename round-trip becomes:

```dart
      final renamed =
          await renameEnv(folder: folder, old: 'prod', new_: 'staging');
      expect(renamed.project.envs.map((e) => e.name).toList(), ['main', 'staging']);
      expect(renamed.followUps, isEmpty); // no .gitlab-ci.yml in this project
```

- [ ] **Step 4: Run**

Run: `cd desktop && flutter analyze && flutter test`
Expected: no issues; all tests pass (goldens unchanged — no layout changed).

Run: `cd desktop && flutter test integration_test/bridge_test.dart -d macos`
Expected: pass.

- [ ] **Step 5: Commit Tasks 4 and 5**

```bash
git add desktop
git commit -m "feat(desktop): rename envs through rdc edit's core and show the follow-ups

Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>"
```

---

### Task 6: Full gate

- [ ] **Step 1: Root gate**

Run from the repo root:
`cargo test --locked && cargo clippy --all-targets --locked -- -D warnings && cargo doc --no-deps --document-private-items --locked`
Expected: all green. A rustdoc warning about a private link in `env.rs` / `ci.rs` doc comments is fixed by turning the link into plain backticks.

- [ ] **Step 2: Desktop gate**

Run: `cd desktop/rust && cargo test && cargo clippy --all-targets -- -D warnings`, then `cd desktop && flutter analyze && flutter test`.
Expected: all green.

- [ ] **Step 3: Live check by hand on a scratch project**

In a temp dir: write an `rdc.toml` with `dev` and `prod`, run `rdc init`, then `cargo run -q -- edit env rename dev sandbox --dry-run` and without `--dry-run`, and `git diff --no-index`-compare `.gitlab-ci.yml` against a fresh copy. Expected: only the region lines naming `dev` differ, and the output lists the GitLab follow-ups. Use the locally built binary (`target/debug/rdc`), not a Homebrew `rdc` on `PATH`.

- [ ] **Step 4: Commit any gate fixes**

```bash
git add -A src tests desktop README.md
git commit -m "chore(edit): satisfy the CI gate

Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>"
```

(Skip if nothing changed.)
