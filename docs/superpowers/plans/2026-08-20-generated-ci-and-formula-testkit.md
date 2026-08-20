# Env-driven GitLab CI + formula testkit Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Generate the env-shaped parts of a project's `.gitlab-ci.yml` from `rdc.toml`, and scaffold a formula/hook test harness that runs the real txscript runtime, so a fresh `rdc init` project has a pipeline that describes it and passes.

**Architecture:** rdc owns two named marker regions inside `.gitlab-ci.yml` — the archive job's `parallel:matrix` and a block of drafted deploy jobs — and splices them from `rdc.toml` on every `rdc init`. Everything outside the markers is the user's. A new pure-function module (`src/cli/gitlab_ci.rs`) renders and splices; `write_gitlab_ci` maps file state to the existing scaffold outcomes. Separately, six embedded Python templates (`testkit/`, `conftest.py`, `pytest.ini`, `requirements-dev.txt`) are scaffolded like `templates/gitlab-ci.yml` already is; the harness builds a txscript payload from the queue's real `schema.json` when one sits beside the formula, and falls back to a synthesized all-string schema when not.

**Tech Stack:** Rust (anyhow, serde, `include_str!`, assert_cmd + predicates + tempfile for CLI tests), Python 3.12 (pytest, txscript), GitLab CI YAML.

**Spec:** `docs/superpowers/specs/2026-08-20-generated-ci-and-formula-testkit-design.md`

## Global Constraints

- **Never put customer names or customer-specific identifiers** — org/division/region codes, real env names, queue/engine/hook slugs, hostnames, URLs, file paths — anywhere in the repo, including tests, fixtures, docs and commit messages. Use neutral placeholders: `dev`, `test`, `prod`, `dev-eu`, `dev-us`, `invoices`, `example.rossum.app`.
- **`templates/gitlab-ci.yml`'s `RDC_VERSION` must stay a pinned tag** (currently `v0.6.0`), bumped only at release. The deploy job runs `rdc sync --allow-deletes --yes` unattended.
- **Edit `templates/gitlab-ci.yml`, never a copy.** It is embedded with `include_str!` and tests compare the written file against it.
- **`rdc.toml` gains no keys.** `ProjectConfig::save` round-trips through the struct and drops what it doesn't model.
- **Supported txscript versions: `1.1.0` and `1.2.0`**, from one code path. `requirements-dev.txt` pins `txscript==1.2.0` and `pytest>=8,<9`.
- **Env names are untrusted.** `parse_env_spec` does not validate them and a hand-written `rdc.toml` can hold anything; every env name emitted into YAML is double-quoted.
- **Do not run repo-wide `cargo fmt`** — this repo is not fmt-clean under local rustfmt, and a `fmt --check` failure is pre-existing.
- **Never `git push`.** Commit locally only.

## File Structure

| File | Responsibility |
|---|---|
| `src/cli/gitlab_ci.rs` **(new)** | Pure functions: render the two region bodies from `cfg.envs`, splice them into an existing file, generate from the embedded template. No filesystem access. |
| `src/cli/mod.rs:463-474` | Declare `pub mod gitlab_ci;`. |
| `src/secrets.rs:103-116` | Extract `env_var_suffix` out of `env_var_for` so the generator can emit `RDC_VAR_SUFFIX` without re-deriving it in shell. |
| `src/cli/init.rs:625-636` | `write_gitlab_ci` takes `&ProjectConfig` and maps file state → `Scaffolded`. |
| `src/cli/init.rs:113-119, 743-763` | Scaffold list and `write_scaffold_files` gain the Python files. |
| `src/cli/init.rs:507-540` | `.gitignore` patterns gain `__pycache__/` and `/.pytest_cache`. |
| `templates/gitlab-ci.yml` | Gains the two marker regions and three static fixes (deploy draft guard, archive credential skip, pytest job comment). |
| `templates/testkit/txscript_eval.py` **(new)** | The harness. The only module that touches txscript internals. |
| `templates/testkit/__init__.py` **(new)** | Re-exports `evaluate_formula`, `load_hook`. |
| `templates/testkit/test_txscript_eval.py` **(new)** | Harness self-tests; also why `pytest -q` collects something on a fresh project. |
| `templates/conftest.py`, `templates/pytest.ini`, `templates/requirements-dev.txt` **(new)** | Make `pytest -q` work from any directory, with the right testpaths and pinned deps. |
| `tests/cli_init.rs` | Replace the byte-identity assertion with the two-half equivalent; cover splicing, scaffolding, and the shipped testkit under pytest. |
| `CLAUDE.md`, `README.md` | Document the region contract and the testkit. |

---

### Task 1: Region rendering

**Files:**
- Create: `src/cli/gitlab_ci.rs`
- Modify: `src/cli/mod.rs:463-474` (add `pub mod gitlab_ci;`)
- Modify: `src/secrets.rs:103-116` (extract `env_var_suffix`)
- Test: `src/cli/gitlab_ci.rs` (inline `#[cfg(test)] mod tests`)

**Interfaces:**
- Consumes: `crate::config::EnvConfig`, `crate::secrets::env_var_for`.
- Produces:
  - `pub fn env_var_suffix(env: &str) -> String` in `src/secrets.rs`
  - `pub const REGION_ARCHIVE_ENVS: &str = "rdc:archive-envs"`
  - `pub const REGION_DEPLOY_JOBS: &str = "rdc:deploy-jobs"`
  - `pub fn render_regions(envs: &BTreeMap<String, EnvConfig>) -> BTreeMap<&'static str, String>`

- [ ] **Step 1: Write the failing tests**

Create `src/cli/gitlab_ci.rs` with only the test module plus `use` lines, so it compiles to a failure about missing items:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::EnvConfig;
    use std::collections::BTreeMap;

    fn envs(names: &[&str]) -> BTreeMap<String, EnvConfig> {
        names
            .iter()
            .enumerate()
            .map(|(i, n)| {
                (
                    (*n).to_string(),
                    EnvConfig {
                        api_base: "https://example.rossum.app/api/v1".to_string(),
                        org_id: 100 + i as u64,
                    },
                )
            })
            .collect()
    }

    #[test]
    fn archive_region_pairs_each_env_with_its_credential_suffix() {
        let r = render_regions(&envs(&["dev", "dev-us"]));
        assert_eq!(
            r[REGION_ARCHIVE_ENVS],
            "- RDC_ENV: \"dev\"\n  \
               RDC_VAR_SUFFIX: \"DEV\"\n\
             - RDC_ENV: \"dev-us\"\n  \
               RDC_VAR_SUFFIX: \"DEV_US\""
        );
    }

    #[test]
    fn archive_region_quotes_names_the_prompt_would_have_rejected() {
        // `--env` and a hand-written rdc.toml do not validate names.
        let r = render_regions(&envs(&["we ird", "a\"b"]));
        assert!(r[REGION_ARCHIVE_ENVS].contains("- RDC_ENV: \"we ird\""));
        assert!(r[REGION_ARCHIVE_ENVS].contains("- RDC_ENV: \"a\\\"b\""));
        assert!(r[REGION_ARCHIVE_ENVS].contains("RDC_VAR_SUFFIX: \"WE_IRD\""));
    }

    #[test]
    fn a_single_env_gets_no_deploy_drafts() {
        let r = render_regions(&envs(&["dev"]));
        assert!(r[REGION_DEPLOY_JOBS].starts_with('#'));
        assert!(!r[REGION_DEPLOY_JOBS].contains("extends: .rdc-deploy"));
    }

    #[test]
    fn each_env_gets_one_draft_with_an_empty_source() {
        let r = render_regions(&envs(&["dev", "prod", "test"]));
        let body = &r[REGION_DEPLOY_JOBS];
        assert_eq!(body.matches("extends: .rdc-deploy").count(), 3);
        assert!(body.contains("\"deploy:test\":\n  extends: .rdc-deploy"));
        assert!(body.contains("    RDC_ENV: \"test\""));
        assert_eq!(body.matches("RDC_SRC: \"\"").count(), 3);
        // BTreeMap order, so the render is stable for the same rdc.toml.
        let dev = body.find("\"deploy:dev\"").unwrap();
        let prod = body.find("\"deploy:prod\"").unwrap();
        let test = body.find("\"deploy:test\"").unwrap();
        assert!(dev < prod && prod < test);
    }

    #[test]
    fn region_bodies_never_start_or_end_with_a_blank_line() {
        // splice re-indents each body line; a stray blank would churn the file.
        for body in render_regions(&envs(&["dev", "test"])).values() {
            assert!(!body.starts_with('\n') && !body.ends_with('\n'), "{body:?}");
        }
    }
}
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test --lib gitlab_ci`
Expected: FAIL — `cannot find function render_regions`, `cannot find value REGION_ARCHIVE_ENVS`. (If `src/cli/mod.rs` has no `pub mod gitlab_ci;` yet the file is not compiled at all; add the declaration in this step so the failure is the intended one.)

- [ ] **Step 3: Extract `env_var_suffix` in `src/secrets.rs`**

Replace the body of `env_var_for` (currently `src/secrets.rs:103-116`) with:

```rust
/// The normalized, shell-safe suffix rdc appends to a per-env credential
/// variable name: ASCII alphanumerics uppercased, every other character `_`
/// (so the shell can export it). `dev-us` -> `DEV_US`.
pub fn env_var_suffix(env: &str) -> String {
    env.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_uppercase()
            } else {
                '_'
            }
        })
        .collect()
}

pub fn env_var_for(env: &str, suffix: &str) -> String {
    format!("RDC_{suffix}_{}", env_var_suffix(env))
}
```

The existing `env_var_for_supports_arbitrary_suffix` test (`src/secrets.rs:616-622`) pins the behaviour, so it must keep passing unchanged.

- [ ] **Step 4: Write the renderer**

Prepend to `src/cli/gitlab_ci.rs`, above the test module:

```rust
//! Generates the env-shaped parts of a project's `.gitlab-ci.yml`.
//!
//! rdc owns two named regions in that file and nothing else: the archive job's
//! `parallel:matrix` (one entry per env) and a block of drafted deploy jobs.
//! Everything outside the markers belongs to the user, which is why
//! regeneration splices instead of rewriting.
//!
//! There is deliberately no promotion chain here. `rdc.toml` records envs in a
//! `BTreeMap` with no ordering meaning, so a draft ships with `RDC_SRC` empty
//! and a guard in the template refuses to run until a human fills it in — a
//! plausible-but-wrong source feeding `migrate --mirror` + `sync
//! --allow-deletes` is worse than a blank.

use crate::config::EnvConfig;
use std::collections::BTreeMap;

/// The archive job's matrix: one entry per env.
pub const REGION_ARCHIVE_ENVS: &str = "rdc:archive-envs";
/// One drafted deploy button per env.
pub const REGION_DEPLOY_JOBS: &str = "rdc:deploy-jobs";

/// Every region rdc owns. Anything else spelled `# >>> rdc:…` is a typo.
pub const REGIONS: [&str; 2] = [REGION_ARCHIVE_ENVS, REGION_DEPLOY_JOBS];

/// Render both region bodies for `envs`. A body is `\n`-separated lines with no
/// leading or trailing newline; [`splice`] applies the marker's indentation.
pub fn render_regions(envs: &BTreeMap<String, EnvConfig>) -> BTreeMap<&'static str, String> {
    BTreeMap::from([
        (REGION_ARCHIVE_ENVS, render_archive_envs(envs)),
        (REGION_DEPLOY_JOBS, render_deploy_jobs(envs)),
    ])
}

fn render_archive_envs(envs: &BTreeMap<String, EnvConfig>) -> String {
    let mut lines: Vec<String> = Vec::with_capacity(envs.len() * 2);
    for name in envs.keys() {
        lines.push(format!("- RDC_ENV: {}", yaml_quote(name)));
        // Emitted rather than re-derived in shell: `tr -c '[:alnum:]' '_'` is
        // locale-dependent for non-ASCII, and we already know the name here.
        lines.push(format!(
            "  RDC_VAR_SUFFIX: {}",
            yaml_quote(&crate::secrets::env_var_suffix(name))
        ));
    }
    lines.join("\n")
}

fn render_deploy_jobs(envs: &BTreeMap<String, EnvConfig>) -> String {
    if envs.len() < 2 {
        return "# No deploy buttons: promoting needs a second env. Add one with\n\
                # `rdc init --env <env>=<api_base>:<org_id>` and this region fills in."
            .to_string();
    }
    let mut out = String::from(
        "# DRAFTS. Each button needs its RDC_SRC filled in before it can be pressed,\n\
         # and the jobs for hand-authored source envs (a dev env people edit) should\n\
         # be deleted -- an archive records those, a deploy would overwrite them.",
    );
    for name in envs.keys() {
        let quoted = yaml_quote(name);
        out.push_str(&format!(
            "\n\n{job}:\n  \
             extends: .rdc-deploy\n  \
             resource_group: {quoted}\n  \
             environment:\n    \
             name: {quoted}\n  \
             variables:\n    \
             RDC_ENV: {quoted}\n    \
             RDC_SRC: \"\"   # TODO: env to promote from",
            job = yaml_quote(&format!("deploy:{name}")),
        ));
    }
    out
}

/// Double-quote a YAML scalar. Env names reach us from `--env` or a
/// hand-written `rdc.toml`, neither of which validates them (only the
/// interactive prompt restricts them to `[A-Za-z0-9_-]`), so an unquoted name
/// could produce invalid YAML or a mis-parsed job name.
fn yaml_quote(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            _ => out.push(c),
        }
    }
    out.push('"');
    out
}
```

- [ ] **Step 5: Run the tests to verify they pass**

Run: `cargo test --lib gitlab_ci && cargo test --lib secrets`
Expected: PASS — 5 new gitlab_ci tests, and every existing `secrets` test still green.

- [ ] **Step 6: Commit**

```bash
git add src/cli/gitlab_ci.rs src/cli/mod.rs src/secrets.rs
git commit -m "feat(ci): render the archive matrix and deploy drafts from rdc.toml"
```

---

### Task 2: Splicing and generation

**Files:**
- Modify: `src/cli/gitlab_ci.rs` (add `regions_present`, `splice`, `generate`)
- Test: `src/cli/gitlab_ci.rs` (inline tests)

**Interfaces:**
- Consumes: `render_regions`, `REGIONS` from Task 1.
- Produces:
  - `pub fn regions_present(existing: &str) -> BTreeSet<String>`
  - `pub fn splice(existing: &str, regions: &BTreeMap<&str, String>) -> Result<Option<String>>` — `Ok(None)` when the file carries no rdc markers
  - `pub fn generate(template: &str, envs: &BTreeMap<String, EnvConfig>) -> Result<String>`

- [ ] **Step 1: Write the failing tests**

Append to the `mod tests` block in `src/cli/gitlab_ci.rs`:

```rust
    const FILE: &str = "\
before
  parallel:
    matrix:
      # >>> rdc:archive-envs  (generated)
      - RDC_ENV: \"stale\"
      # <<< rdc:archive-envs
middle
# >>> rdc:deploy-jobs  (generated)
stale
# <<< rdc:deploy-jobs
after
";

    #[test]
    fn splice_replaces_bodies_and_keeps_everything_else() {
        let out = splice(FILE, &render_regions(&envs(&["dev", "test"])))
            .unwrap()
            .unwrap();
        assert!(out.starts_with("before\n  parallel:\n    matrix:\n"));
        assert!(out.ends_with("# <<< rdc:deploy-jobs\nafter\n"));
        assert!(out.contains("\nmiddle\n"));
        assert!(!out.contains("stale"));
        // the marker lines themselves, comments included, survive verbatim
        assert!(out.contains("      # >>> rdc:archive-envs  (generated)\n"));
    }

    #[test]
    fn splice_indents_a_body_to_its_marker() {
        let out = splice(FILE, &render_regions(&envs(&["dev"]))).unwrap().unwrap();
        assert!(out.contains("\n      - RDC_ENV: \"dev\"\n"));
        assert!(out.contains("\n        RDC_VAR_SUFFIX: \"DEV\"\n"));
    }

    #[test]
    fn splice_leaves_a_hand_written_pipeline_alone() {
        let hand = "stages:\n  - test\npytest:\n  script:\n    - pytest -q\n";
        assert!(splice(hand, &render_regions(&envs(&["dev"]))).unwrap().is_none());
    }

    #[test]
    fn splice_preserves_a_missing_trailing_newline() {
        let no_nl = FILE.trim_end_matches('\n');
        let out = splice(no_nl, &render_regions(&envs(&["dev"]))).unwrap().unwrap();
        assert!(!out.ends_with('\n'));
    }

    #[test]
    fn splice_rejects_a_region_that_is_never_closed() {
        let broken = "# >>> rdc:deploy-jobs\nbody\n";
        let err = format!("{:#}", splice(broken, &render_regions(&envs(&["dev"]))).unwrap_err());
        assert!(err.contains("rdc:deploy-jobs"), "{err}");
        assert!(err.contains("never closed"), "{err}");
    }

    #[test]
    fn splice_rejects_a_close_without_an_open() {
        let broken = "# <<< rdc:deploy-jobs\n";
        let err = format!("{:#}", splice(broken, &render_regions(&envs(&["dev"]))).unwrap_err());
        assert!(err.contains("without opening"), "{err}");
    }

    #[test]
    fn splice_rejects_a_duplicated_region() {
        let broken = "# >>> rdc:deploy-jobs\n# <<< rdc:deploy-jobs\n\
                      # >>> rdc:deploy-jobs\n# <<< rdc:deploy-jobs\n";
        let err = format!("{:#}", splice(broken, &render_regions(&envs(&["dev"]))).unwrap_err());
        assert!(err.contains("more than once"), "{err}");
    }

    #[test]
    fn splice_rejects_a_misspelled_region() {
        // Silently copying it would mean that region never updates again.
        let broken = "# >>> rdc:archive-env\n# <<< rdc:archive-env\n";
        let err = format!("{:#}", splice(broken, &render_regions(&envs(&["dev"]))).unwrap_err());
        assert!(err.contains("unknown rdc region"), "{err}");
    }

    #[test]
    fn splice_rejects_a_region_opened_inside_another() {
        let broken = "# >>> rdc:deploy-jobs\n# >>> rdc:archive-envs\n\
                      # <<< rdc:archive-envs\n# <<< rdc:deploy-jobs\n";
        let err = format!("{:#}", splice(broken, &render_regions(&envs(&["dev"]))).unwrap_err());
        assert!(err.contains("still open"), "{err}");
    }

    #[test]
    fn splice_is_idempotent() {
        let regions = render_regions(&envs(&["dev", "test"]));
        let once = splice(FILE, &regions).unwrap().unwrap();
        let twice = splice(&once, &regions).unwrap().unwrap();
        assert_eq!(once, twice);
    }

    #[test]
    fn generate_fills_the_embedded_template() {
        let out = generate(crate::cli::init::GITLAB_CI_TEMPLATE, &envs(&["dev", "test"])).unwrap();
        assert!(out.contains("- RDC_ENV: \"dev\""));
        assert!(out.contains("\"deploy:test\":"));
        assert!(!out.contains("# TODO: the envs to archive"));
    }

    #[test]
    fn template_carries_both_regions() {
        // The template is ours; a missing marker is a bug in this repo.
        let present = regions_present(crate::cli::init::GITLAB_CI_TEMPLATE);
        for region in REGIONS {
            assert!(present.contains(region), "template is missing {region}");
        }
    }
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test --lib gitlab_ci`
Expected: FAIL — `cannot find function splice` / `regions_present` / `generate`. (`generate_fills_the_embedded_template` and `template_carries_both_regions` will keep failing until Task 3 adds the markers to the template; that is expected and called out again there.)

- [ ] **Step 3: Implement splicing and generation**

Append to `src/cli/gitlab_ci.rs`, above the test module:

```rust
/// Names of the rdc regions whose opening marker appears in `existing`.
pub fn regions_present(existing: &str) -> BTreeSet<String> {
    existing.lines().filter_map(|l| marker_name(l, ">>>")).collect()
}

/// If `line` is an rdc region marker of `kind` (`">>>"` or `"<<<"`), its region
/// name. The marker may carry a trailing comment, and it may be indented.
fn marker_name(line: &str, kind: &str) -> Option<String> {
    let rest = line.trim_start().strip_prefix(&format!("# {kind} rdc:"))?;
    let name: String = rest.chars().take_while(|c| !c.is_whitespace()).collect();
    if name.is_empty() {
        return None;
    }
    Some(format!("rdc:{name}"))
}

/// Replace the body of every rdc region in `existing` with the rendered one,
/// leaving every other byte alone.
///
/// `Ok(None)` means the file carries no rdc markers — a hand-written pipeline,
/// which is left untouched rather than converted. Every error case (a region
/// never closed, closed without opening, duplicated, nested, or misspelled)
/// returns `Err` before producing any output, so the caller has nothing to
/// write: a half-spliced pipeline is worse than a diagnosed one.
pub fn splice(existing: &str, regions: &BTreeMap<&str, String>) -> Result<Option<String>> {
    let mut out: Vec<String> = Vec::new();
    let mut seen: BTreeSet<String> = BTreeSet::new();
    // (region name, line it opened on, its indentation)
    let mut open: Option<(String, usize, String)> = None;
    let mut any = false;

    for (idx, line) in existing.lines().enumerate() {
        let lineno = idx + 1;

        if let Some(name) = marker_name(line, ">>>") {
            let Some(body) = regions.get(name.as_str()) else {
                return Err(anyhow!(
                    "line {lineno}: unknown rdc region '{name}' (known: {})",
                    REGIONS.join(", ")
                ));
            };
            if let Some((open_name, open_line, _)) = &open {
                return Err(anyhow!(
                    "line {lineno}: region '{name}' opens while '{open_name}' from \
                     line {open_line} is still open"
                ));
            }
            if !seen.insert(name.clone()) {
                return Err(anyhow!(
                    "line {lineno}: region '{name}' appears more than once"
                ));
            }
            any = true;
            let indent: String = line.chars().take_while(|c| c.is_whitespace()).collect();
            out.push(line.to_string());
            for body_line in body.lines() {
                if body_line.is_empty() {
                    out.push(String::new());
                } else {
                    out.push(format!("{indent}{body_line}"));
                }
            }
            open = Some((name, lineno, indent));
            continue;
        }

        if let Some(name) = marker_name(line, "<<<") {
            match &open {
                Some((open_name, _, _)) if *open_name == name => {
                    out.push(line.to_string());
                    open = None;
                }
                Some((open_name, open_line, _)) => {
                    return Err(anyhow!(
                        "line {lineno}: region '{name}' closes while '{open_name}' \
                         from line {open_line} is open"
                    ));
                }
                None => {
                    return Err(anyhow!(
                        "line {lineno}: region '{name}' closes without opening"
                    ));
                }
            }
            continue;
        }

        // Lines inside an open region are the previously generated body: dropped.
        if open.is_none() {
            out.push(line.to_string());
        }
    }

    if let Some((name, lineno, _)) = open {
        return Err(anyhow!(
            "region '{name}' opened at line {lineno} is never closed by '# <<< {name}'"
        ));
    }
    if !any {
        return Ok(None);
    }
    let mut joined = out.join("\n");
    // `lines()` drops the final newline; put it back only if it was there.
    if existing.ends_with('\n') {
        joined.push('\n');
    }
    Ok(Some(joined))
}

/// Splice the embedded template for a brand-new (or `--force`d) project.
/// Unlike [`splice`], every region must be present: the template ships with us,
/// so a missing marker is a bug here rather than a user's edit.
pub fn generate(template: &str, envs: &BTreeMap<String, EnvConfig>) -> Result<String> {
    let present = regions_present(template);
    let missing: Vec<&str> = REGIONS.iter().copied().filter(|r| !present.contains(*r)).collect();
    if !missing.is_empty() {
        return Err(anyhow!(
            "the embedded templates/gitlab-ci.yml is missing region marker(s): {}",
            missing.join(", ")
        ));
    }
    splice(template, &render_regions(envs))?
        .ok_or_else(|| anyhow!("the embedded templates/gitlab-ci.yml has no rdc region markers"))
}
```

Extend the imports at the top of the file to:

```rust
use crate::config::EnvConfig;
use anyhow::{anyhow, Result};
use std::collections::{BTreeMap, BTreeSet};
```

Make the template constant reachable from the test: in `src/cli/init.rs:767`, change `const GITLAB_CI_TEMPLATE` to `pub(crate) const GITLAB_CI_TEMPLATE`.

- [ ] **Step 4: Run the tests**

Run: `cargo test --lib gitlab_ci`
Expected: every test PASSES except `generate_fills_the_embedded_template` and `template_carries_both_regions`, which fail with "template is missing rdc:archive-envs" until Task 3. Confirm the failure message is exactly that — it proves `generate`'s guard works.

- [ ] **Step 5: Commit**

```bash
git add src/cli/gitlab_ci.rs src/cli/init.rs
git commit -m "feat(ci): splice rdc-owned regions into an existing pipeline"
```

---

### Task 3: Template regions and the three static fixes

**Files:**
- Modify: `templates/gitlab-ci.yml`
- Test: `src/cli/gitlab_ci.rs` (the two tests left failing by Task 2 now pass)

**Interfaces:**
- Consumes: `render_regions` (Task 1) — the committed region bodies must equal what it renders for `dev`/`test`/`prod`.
- Produces: a template carrying `# >>> rdc:archive-envs` and `# >>> rdc:deploy-jobs`.

- [ ] **Step 1: Replace the archive matrix with the region**

In `templates/gitlab-ci.yml`, replace:

```yaml
  parallel:
    matrix:
      - RDC_ENV: [dev]  # TODO: the envs to archive
```

with:

```yaml
  parallel:
    matrix:
      # >>> rdc:archive-envs  (generated from rdc.toml -- `rdc init --force` to refresh)
      - RDC_ENV: "dev"
        RDC_VAR_SUFFIX: "DEV"
      - RDC_ENV: "prod"
        RDC_VAR_SUFFIX: "PROD"
      - RDC_ENV: "test"
        RDC_VAR_SUFFIX: "TEST"
      # <<< rdc:archive-envs
```

The three envs are alphabetical because `render_regions` iterates a `BTreeMap`; the test in Step 6 compares these bytes against its output.

- [ ] **Step 2: Make the archive job one shell block that can skip**

Replace the archive job's whole `script:` with:

```yaml
  script:
    - |
      set -eu
      # An env whose credentials are not wired up yet is SKIPPED, not failed: a
      # project sets them one env at a time, and a scheduled archive has to stay
      # green meanwhile. One `script:` block on purpose -- `exit 0` then really
      # does stop the job here.
      if [ -z "$(printenv "RDC_TOKEN_$RDC_VAR_SUFFIX" || true)" ] \
      && [ -z "$(printenv "RDC_PASS_$RDC_VAR_SUFFIX" || true)" ]; then
        echo "$RDC_ENV: neither RDC_TOKEN_$RDC_VAR_SUFFIX nor RDC_PASS_$RDC_VAR_SUFFIX is set; skipping."
        exit 0
      fi

      # use-remote: the tenant wins, or the archive silently omits diverged objects.
      rdc sync "$RDC_ENV" --no-push --yes --conflict use-remote

      if ! writeback "chore(archive): $RDC_ENV snapshot ($(date -u +%Y-%m-%d))"; then
        echo "ERROR: could not commit the $RDC_ENV archive back; see the git output above" >&2
        echo "(default branch moved, or a push rule rejected the commit)." >&2
        echo "Nothing was written to Rossum; fix the cause and re-run -- it re-snapshots." >&2
        exit 1
      fi
```

- [ ] **Step 3: Guard the deploy draft**

In `.rdc-deploy`, insert immediately after the `set -eu` that opens its `script:` block:

```yaml
      # A generated deploy job is a DRAFT: rdc cannot know which env promotes
      # into this one, so RDC_SRC ships empty. `${VAR:?}` fires on unset OR
      # empty, and it fires here -- before migrate (offline) or sync (writes) --
      # so an unpressed-ready button cannot touch the tenant.
      : "${RDC_SRC:?this deploy button is still a draft -- set RDC_SRC to the env to promote from}"
```

- [ ] **Step 4: Replace the two hand-written deploy jobs with the region**

Replace everything from the `# The promotion chain.` comment to the end of the file (currently `templates/gitlab-ci.yml:222-241`: that comment, the `# TODO: one job per target env, source -> target` marker, and the `deploy:test:` and `deploy:prod:` blocks) with exactly this — the intro comment is kept, the two hand-written jobs become the generated region:

```yaml
# The promotion chain. `resource_group` serializes every pipeline that deploys
# the same env, so two runs can never interleave writes into one tenant;
# `environment` records the deployment under Operate -> Environments.
# >>> rdc:deploy-jobs  (generated from rdc.toml -- `rdc init --force` to refresh)
# DRAFTS. Each button needs its RDC_SRC filled in before it can be pressed,
# and the jobs for hand-authored source envs (a dev env people edit) should
# be deleted -- an archive records those, a deploy would overwrite them.

"deploy:dev":
  extends: .rdc-deploy
  resource_group: "dev"
  environment:
    name: "dev"
  variables:
    RDC_ENV: "dev"
    RDC_SRC: ""   # TODO: env to promote from

"deploy:prod":
  extends: .rdc-deploy
  resource_group: "prod"
  environment:
    name: "prod"
  variables:
    RDC_ENV: "prod"
    RDC_SRC: ""   # TODO: env to promote from

"deploy:test":
  extends: .rdc-deploy
  resource_group: "test"
  environment:
    name: "test"
  variables:
    RDC_ENV: "test"
    RDC_SRC: ""   # TODO: env to promote from
# <<< rdc:deploy-jobs
```

- [ ] **Step 5: Point the pytest job at the scaffolded testkit and update the header**

Replace the comment above the `pytest:` job — currently "EXAMPLE placeholder for your repo's own checks… No tests in this repo? Delete this job AND the `needs:` in .rdc-deploy." — with:

```yaml
# `rdc init` scaffolds `requirements-dev.txt`, `pytest.ini`, `conftest.py` and a
# `testkit/` whose self-tests run the real txscript runtime, so this job is green
# on a fresh project (with nothing collected, pytest exits 5 and the job is red).
# The deploy buttons depend on it, so a deploy cannot run against a snapshot that
# failed validation.
```

And in the header comment block at the top of the file, after the "Rehearse every new deploy target" paragraph, add:

```yaml
# The regions marked `# >>> rdc:…` are generated from rdc.toml: `rdc init`
# refreshes them in place on every run, including when you add an env, and
# leaves every other line of this file alone. Fill in each deploy draft's
# RDC_SRC, delete the drafts for envs you author by hand, and edit freely
# outside the markers.
```

- [ ] **Step 6: Run the tests to verify they now pass**

Run: `cargo test --lib gitlab_ci`
Expected: PASS, all tests including `template_carries_both_regions` and
`generate_fills_the_embedded_template`. The latter is what pins the committed
region bodies to `render_regions`' output — if the envs in the template's
regions ever drift from alphabetical `dev`/`prod`/`test`, it fails here.

- [ ] **Step 7: Verify the template is still valid YAML**

Run: `python3 -c "import yaml,sys; d=yaml.safe_load(open('templates/gitlab-ci.yml')); print(sorted(k for k in d if not k.startswith('.')))"`
Expected: prints the job list including `archive`, `pytest`, `deploy:dev`, `deploy:prod`, `deploy:test`, `stages`, `variables`, `default`. If `yaml` is not installed, run `python3 -m pip install --user pyyaml` first. This catches an indentation slip in the spliced matrix that no Rust test would see.

- [ ] **Step 8: Commit**

```bash
git add templates/gitlab-ci.yml
git commit -m "feat(templates): mark the env-shaped regions and guard the deploy drafts"
```

---

### Task 4: Wire `write_gitlab_ci` to the generator

**Files:**
- Modify: `src/cli/init.rs:625-636` (`write_gitlab_ci`), `:113-119` (scaffold list), `:743-763` (`write_scaffold_files`)
- Test: `tests/cli_init.rs`

**Interfaces:**
- Consumes: `gitlab_ci::{generate, splice, render_regions, regions_present}` (Tasks 1–2); `Scaffolded` and `write_atomic` (existing, `src/cli/init.rs:19-39`).
- Produces: `fn write_gitlab_ci(root: &Path, cfg: &ProjectConfig, force: bool) -> Result<Scaffolded>`.

- [ ] **Step 1: Write the failing tests**

Replace `init_writes_gitlab_ci_from_the_repo_template` (`tests/cli_init.rs:664-685`) with the two-half equivalent, and add the splice tests:

```rust
/// The static half of the pipeline init writes is the repo's
/// `templates/gitlab-ci.yml` byte-for-byte; the generated regions are
/// `render_regions`' output for this project's envs. Together these keep the
/// copy users read on GitHub and the copy the binary writes from drifting —
/// which is what the old byte-for-byte assertion existed to enforce.
#[test]
fn init_writes_gitlab_ci_from_the_repo_template_outside_the_regions() {
    let dir = TempDir::new().unwrap();
    Command::cargo_bin("rdc")
        .unwrap()
        .current_dir(dir.path())
        .args(["init", "--env", "dev=https://example.rossum.app/api/v1:1"])
        .assert()
        .success();

    let written = std::fs::read_to_string(dir.path().join(".gitlab-ci.yml")).unwrap();
    let template = std::fs::read_to_string("templates/gitlab-ci.yml").unwrap();
    assert_eq!(
        outside_regions(&written),
        outside_regions(&template),
        "everything outside the rdc regions must match templates/gitlab-ci.yml"
    );
    // and the generated half describes THIS project, not the template's example
    assert!(written.contains("- RDC_ENV: \"dev\""));
    assert!(!written.contains("- RDC_ENV: \"test\""));
}

/// Every line that is not inside an `# >>> rdc:…` / `# <<< rdc:…` pair.
fn outside_regions(text: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut inside = false;
    for line in text.lines() {
        let t = line.trim_start();
        if t.starts_with("# >>> rdc:") {
            inside = true;
            out.push(line);
            continue;
        }
        if t.starts_with("# <<< rdc:") {
            inside = false;
            out.push(line);
            continue;
        }
        if !inside {
            out.push(line);
        }
    }
    out
}

#[test]
fn init_adding_an_env_updates_the_regions_and_keeps_hand_edits() {
    let dir = TempDir::new().unwrap();
    Command::cargo_bin("rdc")
        .unwrap()
        .current_dir(dir.path())
        .args(["init", "--env", "dev=https://example.rossum.app/api/v1:1"])
        .assert()
        .success();

    // a job of the user's own, outside the regions
    let path = dir.path().join(".gitlab-ci.yml");
    let mut pipeline = std::fs::read_to_string(&path).unwrap();
    pipeline.push_str("\nmy-own-job:\n  script:\n    - echo mine\n");
    std::fs::write(&path, &pipeline).unwrap();

    Command::cargo_bin("rdc")
        .unwrap()
        .current_dir(dir.path())
        .args(["init", "--env", "test=https://example.rossum.app/api/v1:2"])
        .assert()
        .success();

    let after = std::fs::read_to_string(&path).unwrap();
    assert!(after.contains("my-own-job:"), "hand-added job must survive");
    assert!(after.contains("- RDC_ENV: \"test\""), "new env must reach the matrix");
    assert!(after.contains("\"deploy:test\":"), "new env must get a draft");
    // one env before, two now: the drafts region stops being a bare comment
    assert!(after.contains("\"deploy:dev\":"));
}

#[test]
fn init_never_touches_a_pipeline_without_rdc_markers() {
    let dir = TempDir::new().unwrap();
    let hand = "stages:\n  - test\nmine:\n  script:\n    - echo hi\n";
    Command::cargo_bin("rdc")
        .unwrap()
        .current_dir(dir.path())
        .args(["init", "--env", "dev=https://example.rossum.app/api/v1:1"])
        .assert()
        .success();
    std::fs::write(dir.path().join(".gitlab-ci.yml"), hand).unwrap();

    Command::cargo_bin("rdc")
        .unwrap()
        .current_dir(dir.path())
        .args(["init", "--env", "test=https://example.rossum.app/api/v1:2"])
        .assert()
        .success();

    assert_eq!(
        std::fs::read_to_string(dir.path().join(".gitlab-ci.yml")).unwrap(),
        hand,
        "a hand-written pipeline has no rdc regions and must be left alone"
    );
}

#[test]
fn init_force_regenerates_a_markerless_pipeline() {
    let dir = TempDir::new().unwrap();
    Command::cargo_bin("rdc")
        .unwrap()
        .current_dir(dir.path())
        .args(["init", "--env", "dev=https://example.rossum.app/api/v1:1"])
        .assert()
        .success();
    std::fs::write(dir.path().join(".gitlab-ci.yml"), "mine\n").unwrap();

    Command::cargo_bin("rdc")
        .unwrap()
        .current_dir(dir.path())
        .args(["init", "--force"])
        .assert()
        .success()
        .stdout(predicate::str::contains(".gitlab-ci.yml   rewritten"));

    let after = std::fs::read_to_string(dir.path().join(".gitlab-ci.yml")).unwrap();
    assert!(after.contains("- RDC_ENV: \"dev\""));
}
```

Also update the two existing tests that assert byte-equality with the template: `init_force_without_env_regenerates_scaffold_files` (`tests/cli_init.rs:707-765`, the `assert_eq!` at :749-752) and the `write_scaffold_files` unit test (`src/cli/init.rs:959-977`, the `assert_eq!` at :971-974). Both should compare against `gitlab_ci::generate(GITLAB_CI_TEMPLATE, &cfg.envs).unwrap()` instead of the raw template.

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test --test cli_init`
Expected: FAIL — the new tests fail because `write_gitlab_ci` still writes the template verbatim (`- RDC_ENV: "test"` is present when it should not be, hand edits are lost, etc.).

- [ ] **Step 3: Rewrite `write_gitlab_ci`**

Replace `src/cli/init.rs:625-636` with:

```rust
/// Write the GitLab CI pipeline at `<root>/.gitlab-ci.yml`.
///
/// The body is the repo's `templates/gitlab-ci.yml`, embedded at compile time
/// so the copy users read on GitHub and the copy this binary writes cannot
/// drift — with the two `# >>> rdc:…` regions filled in from `cfg` (the archive
/// matrix and one drafted deploy button per env).
///
/// | file state                  | action                        | outcome     |
/// |-----------------------------|-------------------------------|-------------|
/// | absent                      | write the generated template  | `Created`   |
/// | has markers                 | splice; bytes equal           | `Unchanged` |
/// | has markers                 | splice; bytes differ          | `Merged`    |
/// | no markers, no `--force`    | leave alone                   | `Unchanged` |
/// | no markers, `--force`       | regenerate the whole file     | `Rewritten` |
///
/// A markered file is spliced even under `--force`: the lines outside the
/// markers are the user's, and `Merged` already means "rdc-owned lines
/// refreshed, user lines kept" for `.gitignore`. To take a newer binary's
/// static half, delete the file and re-run `rdc init`.
///
/// With no envs defined (a hand-emptied `rdc.toml`), the template is written
/// verbatim: an empty `parallel:matrix` is not valid YAML, and its committed
/// example is.
fn write_gitlab_ci(root: &Path, cfg: &ProjectConfig, force: bool) -> Result<Scaffolded> {
    let path = root.join(".gitlab-ci.yml");
    if cfg.envs.is_empty() {
        return write_template_file(&path, GITLAB_CI_TEMPLATE, force);
    }

    let generated =
        || crate::cli::gitlab_ci::generate(GITLAB_CI_TEMPLATE, &cfg.envs);

    if !path.exists() {
        write_atomic(&path, generated()?.as_bytes())?;
        return Ok(Scaffolded::Created);
    }

    let existing = std::fs::read(&path).with_context(|| format!("reading {}", path.display()))?;
    // A hand-edited pipeline that isn't valid UTF-8 can't be spliced; treat it
    // like write_template_file does — byte comparison, never a parse.
    let Ok(text) = String::from_utf8(existing.clone()) else {
        return write_template_file_bytes(&path, generated()?.as_bytes(), &existing, force);
    };

    match crate::cli::gitlab_ci::splice(&text, &crate::cli::gitlab_ci::render_regions(&cfg.envs))
        .with_context(|| format!("updating the rdc regions in {}", path.display()))?
    {
        Some(spliced) => {
            if spliced.as_bytes() == existing.as_slice() {
                Ok(Scaffolded::Unchanged)
            } else {
                write_atomic(&path, spliced.as_bytes())?;
                Ok(Scaffolded::Merged)
            }
        }
        None => write_template_file_bytes(&path, generated()?.as_bytes(), &existing, force),
    }
}

/// `write_template_file`'s force semantics against bytes already in hand.
fn write_template_file_bytes(
    path: &Path,
    body: &[u8],
    existing: &[u8],
    force: bool,
) -> Result<Scaffolded> {
    if !force {
        return Ok(Scaffolded::Unchanged);
    }
    if existing == body {
        return Ok(Scaffolded::Unchanged);
    }
    write_atomic(path, body)?;
    Ok(Scaffolded::Rewritten)
}
```

- [ ] **Step 4: Update the two call sites**

`src/cli/init.rs:118` becomes:

```rust
        (".gitlab-ci.yml", write_gitlab_ci(&cwd, &cfg, force)?),
```

and in `write_scaffold_files` (`src/cli/init.rs:761`), move the call below the `cfg` that `write_readme` already builds and pass it:

```rust
    write_readme(cwd, &cfg, false)?;
    write_gitlab_ci(cwd, &cfg, false)?;
```

- [ ] **Step 5: Run the tests**

Run: `cargo test --test cli_init && cargo test --lib`
Expected: PASS.

- [ ] **Step 6: Commit**

```bash
git add src/cli/init.rs tests/cli_init.rs
git commit -m "feat(init): generate the pipeline's env regions instead of shipping placeholders"
```

---

### Task 5: The testkit templates

**Files:**
- Create: `templates/testkit/txscript_eval.py`, `templates/testkit/__init__.py`, `templates/testkit/test_txscript_eval.py`
- Create: `templates/conftest.py`, `templates/pytest.ini`, `templates/requirements-dev.txt`
- Test: the testkit's own pytest suite, run by hand in this task and by `cargo test` in Task 7

**Interfaces:**
- Consumes: nothing from earlier tasks.
- Produces: `evaluate_formula(formula_path, *, annotation_id=1, status="exported", rows=None, **field_values)` and `load_hook(hook_path)`, exported from the `testkit` package.

This code is verified: 17/17 self-tests green under txscript 1.1.0 **and** 1.2.0, and it evaluates 558 real formulas across a real multi-env snapshot with zero exceptions. Copy it exactly.

- [ ] **Step 1: Create the harness**

`templates/testkit/txscript_eval.py`:

```python
"""Test helpers that exercise Rossum schema formulas and function hooks against
the REAL txscript runtime (`pip install txscript`), so tests verify exactly what
Rossum executes -- no stubs, no reimplemented logic.

This module is the ONLY place that touches txscript internals; a txscript
version bump should only require changes here. It supports txscript 1.1.0 and
1.2.0 from one code path.

When the formula sits in an rdc snapshot -- `<queue>/formulas/<field_id>.py`
next to `<queue>/schema.json` -- the queue's real schema is used, so field types
(number, date, boolean, enum), the line-item table structure, and the set of
fields that exist are the tenant's own. A formula with no schema beside it falls
back to a synthesized all-string schema, which is enough for testing a formula
written inline in a test.
"""
from __future__ import annotations

import datetime
import importlib.util
import json
import pathlib
from typing import Any

from txscript.formula import Formula
from txscript.txscript import TxScript

try:  # txscript >= 1.2.0 wraps a formula result; 1.1.0 has no such class
    from txscript.computed import EvalResult as _EvalResult
except ImportError:
    _EvalResult = None

# txscript >= 1.2.0 reads these annotation keys unconditionally (and
# `payload["document"]`); 1.1.0 reads none of them. Sending both shapes at once
# is what makes one payload work on both versions.
_TIMESTAMPS = (
    "created_at",
    "modified_at",
    "exported_at",
    "confirmed_at",
    "assigned_at",
    "export_failed_at",
    "deleted_at",
    "rejected_at",
    "purged_at",
)


def evaluate_formula(
    formula_path: str | pathlib.Path,
    *,
    annotation_id: int = 1,
    status: str = "exported",
    rows: dict[str, list] | None = None,
    **field_values: Any,
) -> Any:
    """Evaluate a schema-field formula file with given input field values.

    Reads the real formula source, builds a minimal annotation payload carrying
    `field_values`, and returns the value the formula produces under the real
    txscript runtime.

    With the queue's `schema.json` beside the formula, every field carries its
    real type -- so a `number` field arrives as a number and a `date` field must
    be given ISO `YYYY-MM-DD` (or a `datetime.date`), which is the only form the
    runtime parses. A field name that the schema does not define is an error
    rather than a silently invented empty field.

    Without a schema (a formula written to `tmp_path` in a test), every field is
    a string, only the fields the formula references exist, and referenced
    inputs left unsupplied default to "".

    Table columns: pass `rows={"line_items": [{...}, {...}]}` to evaluate a
    column formula once per row, which returns a list. Passing the column values
    as bare keyword arguments evaluates a single implicit row and returns that
    row's scalar.

    `annotation_id` is exposed to the formula as `annotation.id`, and
    `status` as `annotation.status`.
    """
    path = pathlib.Path(formula_path).resolve()
    schema_id = path.stem
    source = path.read_text()

    schema_content = _load_schema(path)
    values = dict(field_values)
    explicit_rows = rows is not None
    rows = dict(rows or {})

    if schema_content is None:
        formula = Formula(schema_id, source)
        # The output field plus every input field the formula references
        # (deduplicated: a formula may legitimately reference its own field).
        schema_content = _synthesized_schema(sorted(formula.dependencies | {schema_id}))
        index = _index(schema_content)
        multivalue_id = None
    else:
        index = _index(schema_content)
        if schema_id not in index:
            raise AssertionError(
                f"{path.parents[1] / 'schema.json'} defines no field '{schema_id}'; "
                f"the formula file has no field to compute"
            )
        multivalue_id = _multivalue_of(schema_id, index)
        # The parent multivalue id is how txscript rewrites a column formula's
        # `_index` dependency, so it must be passed for the checks below to see
        # the same dependency set the runtime does.
        formula = Formula(schema_id, source, multivalue_id)
        _check_fields(path, index, formula, values, rows)

    if multivalue_id is not None:
        values, rows = _route_columns(
            schema_id, multivalue_id, index, values, rows, explicit_rows
        )

    content = _content(schema_content, values, rows, [1000])
    t = TxScript.from_payload(_payload(schema_content, content, annotation_id, status))
    if t.annotation is not None and not hasattr(t.annotation, "id"):
        # txscript 1.1.0's Annotation wrapper has no `id`; the live Rossum
        # formula runtime does expose one, and 1.2.0 sets it itself.
        t.annotation.id = annotation_id

    if multivalue_id is None:
        # _readonly_context() stops formula.evaluate from writing field updates
        # back; it is a txscript-private API and the most likely breakage point
        # on a version bump.
        with t.field._readonly_context():
            return _unwrap(formula.evaluate(t))

    # A column formula evaluates once per row, inside that row's context --
    # the same three calls txscript's own eval_strings makes.
    results = []
    for row in t.field._get_field(multivalue_id).get_value():
        with row._row_formula_context(t) as row_t:
            with row._field_context(row._get_field(schema_id)):
                with t.field._readonly_context():
                    results.append(_unwrap(formula.evaluate(row_t)))
    if explicit_rows:
        return results
    return results[0] if results else None


def load_hook(hook_path: str | pathlib.Path):
    """Import a (possibly hyphenated) Rossum function-hook .py file as a module,
    with the real txscript package importable, and return the module so its
    helper functions / rossum_hook_request_handler can be called directly.
    """
    path = pathlib.Path(hook_path)
    spec = importlib.util.spec_from_file_location(path.stem.replace("-", "_"), path)
    if spec is None or spec.loader is None:
        raise FileNotFoundError(f"Cannot load hook module from {path}")
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


# --- schema ---------------------------------------------------------------


def _load_schema(formula_path: pathlib.Path) -> list | None:
    """The queue's schema content, if the formula sits in an rdc snapshot.

    rdc lays a queue out as `<queue>/schema.json` alongside
    `<queue>/formulas/<field_id>.py`, so the schema is one directory up.
    """
    if len(formula_path.parents) < 2:
        return None
    candidate = formula_path.parents[1] / "schema.json"
    if not candidate.is_file():
        return None
    return json.loads(candidate.read_text())["content"]


def _synthesized_schema(field_ids: list[str]) -> list:
    return [
        {
            "category": "section",
            "id": "section",
            "children": [
                {"category": "datapoint", "id": fid, "type": "string"} for fid in field_ids
            ],
        }
    ]


def _index(nodes: list, parent: dict | None = None, out: dict | None = None) -> dict:
    """schema_id -> (node, parent node) for every node in the schema tree."""
    out = {} if out is None else out
    for node in nodes:
        out[node["id"]] = (node, parent)
        children = node.get("children")
        if isinstance(children, dict):  # a multivalue carries a single child
            children = [children]
        if children:
            _index(children, node, out)
    return out


def _multivalue_of(schema_id: str, index: dict) -> str | None:
    """The multivalue a table column belongs to, or None for a plain field."""
    parent = index[schema_id][1]
    if parent is None or parent["category"] != "tuple":
        return None
    return index[parent["id"]][1]["id"]


def _check_fields(path, index, formula, values, rows) -> None:
    """Reject names the queue's schema does not define. A synthesized schema
    invents whatever it is handed, which lets a typo -- or a formula referencing
    a field since deleted -- pass a test and fail in the tenant.
    """
    schema_json = path.parents[1] / "schema.json"
    row_columns = {column for row in rows.values() for entry in row if isinstance(entry, dict) for column in entry}
    for label, names in (
        ("input", set(values)),
        ("table", set(rows)),
        ("row column", row_columns),
    ):
        unknown = sorted(names - set(index))
        if unknown:
            raise AssertionError(f"{schema_json} defines no {label} field(s): {unknown}")
    absent = sorted(formula.dependencies - set(index))
    if absent:
        raise AssertionError(
            f"{path.name} references field(s) absent from {schema_json}: {absent}"
        )


def _route_columns(schema_id, multivalue_id, index, values, rows, explicit_rows):
    """Column values passed as bare keyword arguments become one implicit row."""
    if explicit_rows and multivalue_id not in rows:
        # Naming some other table would otherwise leave this one with the
        # default single empty row and return a puzzling empty result.
        raise AssertionError(
            f"'{schema_id}' is a column of '{multivalue_id}', but rows were given for "
            f"{sorted(rows)}; pass rows={{'{multivalue_id}': [...]}}"
        )
    tuple_id = index[schema_id][1]["id"]
    columns = {
        name: value
        for name, value in values.items()
        if (index[name][1] or {}).get("id") == tuple_id
    }
    if not columns:
        rows.setdefault(multivalue_id, [{}])
        return values, rows
    if multivalue_id in rows:
        raise AssertionError(
            f"pass the column value(s) {sorted(columns)} either as keyword arguments "
            f"or inside rows[{multivalue_id!r}], not both"
        )
    remaining = {k: v for k, v in values.items() if k not in columns}
    rows[multivalue_id] = [columns]
    return remaining, rows


# --- payload --------------------------------------------------------------


def _serialize(value: Any) -> str:
    """Annotation content stores strings. A date is normalized to ISO, the only
    form the runtime parses for a `date` field.
    """
    if value is None:
        return ""
    if isinstance(value, datetime.datetime):
        return value.date().isoformat()
    if isinstance(value, datetime.date):
        return value.isoformat()
    if isinstance(value, bool):
        return "True" if value else "False"
    return str(value)


def _datapoint(node, value, counter):
    counter[0] += 1
    return {
        "id": counter[0],
        "schema_id": node["id"],
        "category": "datapoint",
        "content": {"value": _serialize(value), "normalized_value": None},
    }


def _content(nodes, values, rows, counter) -> list:
    """Mirror the schema tree as annotation content, filling in the given values."""
    out = []
    for node in nodes:
        category = node["category"]
        if category == "datapoint":
            out.append(_datapoint(node, values.get(node["id"]), counter))
            continue
        counter[0] += 1
        base = {"id": counter[0], "schema_id": node["id"], "category": category}
        if category == "multivalue":
            child = node["children"]
            out.append(
                {
                    **base,
                    "children": [_row(child, row, counter) for row in rows.get(node["id"], [])],
                }
            )
        else:  # section, or a tuple outside a multivalue
            out.append({**base, "children": _content(node.get("children", []), values, rows, counter)})
    return out


def _row(child, row, counter):
    if child["category"] != "tuple":
        # a multivalue of bare datapoints: the row IS the value
        return _datapoint(child, row, counter)
    counter[0] += 1
    return {
        "id": counter[0],
        "schema_id": child["id"],
        "category": "tuple",
        "children": _content(child["children"], row, {}, counter),
    }


def _payload(schema_content, annotation_content, annotation_id, status) -> dict:
    return {
        "event": "annotation_content",
        "schemas": [{"content": schema_content}],
        # status + url make txscript build the Annotation object (it returns
        # None otherwise); content carries the field values.
        "annotation": {
            "id": annotation_id,
            "url": f"https://example.rossum.app/api/v1/annotations/{annotation_id}",
            "status": status,
            "content": annotation_content,
            "automated": False,
            "automatically_rejected": False,
            "einvoice": False,
            "metadata": {},
            **{key: None for key in _TIMESTAMPS},
        },
        "document": {
            "id": 1,
            "url": "https://example.rossum.app/api/v1/documents/1",
            "arrived_at": None,
            "created_at": None,
            "original_file_name": "example.pdf",
            "metadata": {},
            "mime_type": "application/pdf",
        },
    }


def _unwrap(result: Any) -> Any:
    """txscript 1.2.0 wraps a formula result in EvalResult; 1.1.0 returns it raw.

    Tested by type, never by `getattr(result, "value", result)`: a formula that
    returns a field directly (`default_to(field.a, field.b)`) hands back a
    FieldValueBase proxy whose `.value` is the datapoint's raw *string*, so the
    duck-typed form would silently strip a date or number back down to text.
    """
    if _EvalResult is not None and isinstance(result, _EvalResult):
        return result.value
    return result
```

- [ ] **Step 2: Create the package export**

`templates/testkit/__init__.py`:

```python
from .txscript_eval import evaluate_formula, load_hook

__all__ = ["evaluate_formula", "load_hook"]
```

- [ ] **Step 3: Create the self-tests**

`templates/testkit/test_txscript_eval.py`:

```python
"""Self-tests for the testkit harness, independent of any repo formula/hook.

These also guarantee `pytest -q` collects something in a project that has not
written any tests of its own yet: with nothing collected pytest exits 5, and the
pipeline's test job goes red.
"""
import datetime
import json
import textwrap

import pytest

from testkit import evaluate_formula, load_hook


# --- no schema beside the formula: every field is a string ------------------


def test_evaluate_formula_runs_real_txscript(tmp_path):
    formula = tmp_path / "demo.py"
    formula.write_text('default_to(field.a, "").upper()')
    assert evaluate_formula(formula, a="hello") == "HELLO"
    assert evaluate_formula(formula) == ""  # unset input defaults to empty string


def test_evaluate_formula_uses_dependencies(tmp_path):
    formula = tmp_path / "pick.py"
    formula.write_text('default_to(field.manual, "").strip() or default_to(field.captured, "")')
    assert evaluate_formula(formula, manual="M", captured="C") == "M"
    assert evaluate_formula(formula, manual="", captured="C") == "C"


def test_evaluate_formula_exposes_annotation_id(tmp_path):
    # The live Rossum formula runtime exposes annotation.id; the harness must too.
    formula = tmp_path / "name.py"
    formula.write_text('f"{field.branch or \'X\'}_{annotation.id}.json"')
    assert evaluate_formula(formula, annotation_id=42, branch="MAIN") == "MAIN_42.json"
    assert evaluate_formula(formula, branch="MAIN") == "MAIN_1.json"  # default id


def test_evaluate_formula_exposes_annotation_status(tmp_path):
    formula = tmp_path / "state.py"
    formula.write_text("annotation.status")
    assert evaluate_formula(formula) == "exported"
    assert evaluate_formula(formula, status="to_review") == "to_review"


def test_load_hook_imports_with_real_txscript(tmp_path):
    hook = tmp_path / "my-hook.py"
    hook.write_text(textwrap.dedent('''
        from txscript import is_set
        def keep(v):
            return v if is_set(v) else "fallback"
    '''))
    m = load_hook(hook)
    assert m.keep("x") == "x"
    assert m.keep("") == "fallback"


# --- with the queue's real schema: real field types ------------------------


def queue(tmp_path, **formulas):
    """An rdc-shaped queue directory: schema.json plus formulas/<field_id>.py.

    The schema mirrors the shape rdc snapshots: typed datapoints in a section,
    and a line-item table whose columns include a formula column.
    """
    root = tmp_path / "queues" / "invoices"
    (root / "formulas").mkdir(parents=True)
    schema = {
        "content": [
            {
                "category": "section",
                "id": "totals_section",
                "children": [
                    {"category": "datapoint", "id": "amount", "type": "number"},
                    {"category": "datapoint", "id": "doubled", "type": "number"},
                    {"category": "datapoint", "id": "due_date", "type": "date"},
                    {"category": "datapoint", "id": "due_date_out", "type": "date"},
                    {"category": "datapoint", "id": "terms", "type": "enum", "enum_value_type": "string"},
                    {"category": "datapoint", "id": "terms_out", "type": "string"},
                    {"category": "datapoint", "id": "row_total", "type": "number"},
                ],
            },
            {
                "category": "section",
                "id": "line_items_section",
                "children": [
                    {
                        "category": "multivalue",
                        "id": "line_items",
                        "children": {
                            "category": "tuple",
                            "id": "line_item",
                            "children": [
                                {"category": "datapoint", "id": "item_qty", "type": "number"},
                                {"category": "datapoint", "id": "item_price", "type": "number"},
                                {"category": "datapoint", "id": "item_total", "type": "number"},
                            ],
                        },
                    }
                ],
            },
            {
                "category": "section",
                "id": "charges_section",
                "children": [
                    {
                        "category": "multivalue",
                        "id": "charges",
                        "children": {
                            "category": "tuple",
                            "id": "charge",
                            "children": [
                                {"category": "datapoint", "id": "charge_amount", "type": "number"}
                            ],
                        },
                    }
                ],
            },
        ]
    }
    (root / "schema.json").write_text(json.dumps(schema))
    for field_id, source in formulas.items():
        (root / "formulas" / f"{field_id}.py").write_text(source)
    return root / "formulas"


def test_number_field_arrives_as_a_number(tmp_path):
    """An all-string schema makes this string repetition ('1010'), not arithmetic."""
    formulas = queue(tmp_path, doubled="field.amount * 2")
    assert evaluate_formula(formulas / "doubled.py", amount="10") == 20.0


def test_date_field_arrives_as_a_date(tmp_path):
    formulas = queue(tmp_path, due_date_out="field.due_date")
    assert evaluate_formula(formulas / "due_date_out.py", due_date="2026-09-30") == datetime.date(
        2026, 9, 30
    )
    # a date object is accepted and normalized to the ISO form the runtime parses
    assert evaluate_formula(
        formulas / "due_date_out.py", due_date=datetime.date(2026, 9, 30)
    ) == datetime.date(2026, 9, 30)


def test_a_date_the_runtime_cannot_parse_reads_empty(tmp_path):
    """The schema's display format (M/D/YYYY) is not what a date field stores;
    the runtime parses ISO only, and anything else reads as empty -- exactly as
    it does in the tenant."""
    formulas = queue(tmp_path, due_date_out='"empty" if is_empty(field.due_date) else "set"')
    assert evaluate_formula(formulas / "due_date_out.py", due_date="9/30/2026") == "empty"
    assert evaluate_formula(formulas / "due_date_out.py", due_date="2026-09-30") == "set"


def test_enum_field_behaves_as_its_value_type(tmp_path):
    formulas = queue(tmp_path, terms_out="default_to(field.terms, \"\").upper()")
    assert evaluate_formula(formulas / "terms_out.py", terms="net30") == "NET30"


def test_unknown_input_field_is_rejected(tmp_path):
    """A synthesized schema invents whatever it is handed, so a typo passes."""
    formulas = queue(tmp_path, doubled="field.amount * 2")
    with pytest.raises(AssertionError, match="no input field"):
        evaluate_formula(formulas / "doubled.py", amountt="10")


def test_formula_referencing_an_absent_field_is_rejected(tmp_path):
    formulas = queue(tmp_path, doubled="field.gone * 2")
    with pytest.raises(AssertionError, match="absent from"):
        evaluate_formula(formulas / "doubled.py")


def test_column_formula_evaluates_one_implicit_row(tmp_path):
    formulas = queue(tmp_path, item_total="field.item_qty * field.item_price")
    assert evaluate_formula(formulas / "item_total.py", item_qty="3", item_price="4") == 12.0


def test_column_formula_evaluates_every_explicit_row(tmp_path):
    formulas = queue(tmp_path, item_total="field.item_qty * field.item_price")
    assert evaluate_formula(
        formulas / "item_total.py",
        rows={"line_items": [{"item_qty": "3", "item_price": "4"}, {"item_qty": "2", "item_price": "5"}]},
    ) == [12.0, 10.0]


def test_whole_column_is_reachable_from_a_plain_field(tmp_path):
    """`.all_values` needs the real table structure; a synthesized schema has none."""
    formulas = queue(tmp_path, row_total="sum(field.item_total.all_values)")
    assert evaluate_formula(
        formulas / "row_total.py",
        rows={"line_items": [{"item_total": "10"}, {"item_total": "20"}]},
    ) == 30.0


def test_wrong_table_named_in_rows_is_rejected(tmp_path):
    """A schema can hold several tables. Naming the wrong one would leave this
    column's table with the default empty row and return a puzzling empty
    result instead of an error."""
    formulas = queue(tmp_path, item_total="field.item_qty * field.item_price")
    with pytest.raises(AssertionError, match="column of 'line_items'"):
        evaluate_formula(formulas / "item_total.py", rows={"charges": [{"charge_amount": "1"}]})


def test_table_name_absent_from_the_schema_is_rejected(tmp_path):
    formulas = queue(tmp_path, item_total="field.item_qty * field.item_price")
    with pytest.raises(AssertionError, match="no table field"):
        evaluate_formula(formulas / "item_total.py", rows={"not_a_table": [{}]})


def test_unknown_column_inside_a_row_is_rejected(tmp_path):
    formulas = queue(tmp_path, item_total="field.item_qty * field.item_price")
    with pytest.raises(AssertionError, match="no row column field"):
        evaluate_formula(formulas / "item_total.py", rows={"line_items": [{"item_qtyy": "3"}]})
```

- [ ] **Step 4: Create the three pytest support files**

`templates/conftest.py`:

```python
import os
import sys

# Make the repo root importable so tests can `from testkit import ...`
# regardless of the directory pytest is invoked from.
sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
```

`templates/pytest.ini`:

```ini
[pytest]
testpaths = envs testkit
python_files = test_*.py
```

`templates/requirements-dev.txt`:

```
pytest>=8,<9

# The real Rossum formula runtime. Pinned: 1.2.0 dereferences payload["document"]
# and returns an EvalResult where 1.1.0 returned the value raw, so an unpinned
# bump can change what a formula test asserts. testkit/txscript_eval.py handles
# both 1.1.0 and 1.2.0; 1.2.0 is the default because it exposes annotation.id
# natively, as the live runtime does.
txscript==1.2.0

# Runtime libraries your hooks import. Tests load hook .py files as modules, so a
# missing one is a collection ERROR, not a skip. Add what your hooks need, e.g.:
# requests>=2,<3
# pydantic>=2,<3
```

- [ ] **Step 5: Run the self-tests against both supported txscript versions**

```bash
cd "$(mktemp -d)" && python3 -m venv v && ./v/bin/python -m pip install -q 'pytest>=8,<9' 'txscript==1.2.0'
mkdir -p testkit
cp "$OLDPWD"/templates/testkit/*.py testkit/
cp "$OLDPWD"/templates/conftest.py "$OLDPWD"/templates/pytest.ini .
./v/bin/python -m pytest -q
./v/bin/python -m pip install -q 'txscript==1.1.0' && ./v/bin/python -m pytest -q
```

Expected: `17 passed` both times. If a test fails on 1.1.0 only, the cause is almost certainly `_unwrap` — it must test `isinstance(result, EvalResult)`, never `getattr(result, "value", result)`, because a formula that returns a field directly hands back a proxy whose `.value` is the datapoint's raw string.

- [ ] **Step 6: Commit**

```bash
git add templates/testkit templates/conftest.py templates/pytest.ini templates/requirements-dev.txt
git commit -m "feat(testkit): a schema-aware formula harness on the real txscript runtime"
```

---

### Task 6: Scaffold the Python files from `rdc init`

**Files:**
- Modify: `src/cli/init.rs` (new `include_str!` constants, new writers, scaffold list, `write_scaffold_files`, `.gitignore` patterns, `CLAUDE_MD_TEMPLATE` layout block)
- Test: `tests/cli_init.rs`

**Interfaces:**
- Consumes: `write_template_file` and `Scaffolded` (existing); the templates from Task 5.
- Produces: `fn write_testkit(root: &Path, force: bool) -> Result<Vec<(&'static str, Scaffolded)>>`.

- [ ] **Step 1: Write the failing tests**

Add to `tests/cli_init.rs`:

```rust
#[test]
fn init_scaffolds_the_testkit_byte_for_byte() {
    let dir = TempDir::new().unwrap();
    Command::cargo_bin("rdc")
        .unwrap()
        .current_dir(dir.path())
        .args(["init", "--env", "dev=https://example.rossum.app/api/v1:1"])
        .assert()
        .success();

    for rel in [
        "testkit/__init__.py",
        "testkit/txscript_eval.py",
        "testkit/test_txscript_eval.py",
        "conftest.py",
        "pytest.ini",
        "requirements-dev.txt",
    ] {
        let written = std::fs::read_to_string(dir.path().join(rel)).unwrap();
        let template = std::fs::read_to_string(format!("templates/{rel}")).unwrap();
        assert_eq!(written, template, "{rel} must match templates/{rel}");
    }
}

#[test]
fn init_gitignores_the_python_caches() {
    let dir = TempDir::new().unwrap();
    Command::cargo_bin("rdc")
        .unwrap()
        .current_dir(dir.path())
        .args(["init", "--env", "dev=https://example.rossum.app/api/v1:1"])
        .assert()
        .success();
    let gitignore = std::fs::read_to_string(dir.path().join(".gitignore")).unwrap();
    assert!(gitignore.contains("__pycache__/"));
    assert!(gitignore.contains("/.pytest_cache"));
}

#[test]
fn init_does_not_clobber_an_existing_testkit() {
    let dir = TempDir::new().unwrap();
    std::fs::create_dir(dir.path().join("testkit")).unwrap();
    std::fs::write(dir.path().join("testkit/txscript_eval.py"), "mine\n").unwrap();
    std::fs::write(dir.path().join("requirements-dev.txt"), "mine\n").unwrap();

    Command::cargo_bin("rdc")
        .unwrap()
        .current_dir(dir.path())
        .args(["init", "--env", "dev=https://example.rossum.app/api/v1:1"])
        .assert()
        .success();

    assert_eq!(
        std::fs::read_to_string(dir.path().join("testkit/txscript_eval.py")).unwrap(),
        "mine\n"
    );
    assert_eq!(
        std::fs::read_to_string(dir.path().join("requirements-dev.txt")).unwrap(),
        "mine\n"
    );
}

#[test]
fn init_force_reports_every_testkit_file() {
    let dir = TempDir::new().unwrap();
    Command::cargo_bin("rdc")
        .unwrap()
        .current_dir(dir.path())
        .args(["init", "--env", "dev=https://example.rossum.app/api/v1:1"])
        .assert()
        .success();

    Command::cargo_bin("rdc")
        .unwrap()
        .current_dir(dir.path())
        .args(["init", "--force"])
        .assert()
        .success()
        .stdout(predicate::str::contains("testkit/txscript_eval.py"))
        .stdout(predicate::str::contains("requirements-dev.txt"));
}
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test --test cli_init init_scaffolds_the_testkit_byte_for_byte`
Expected: FAIL — `No such file or directory` for `testkit/__init__.py`.

- [ ] **Step 3: Embed the templates and write them**

Add next to `GITLAB_CI_TEMPLATE` (`src/cli/init.rs:767`):

```rust
/// The Python test harness `rdc init` scaffolds, embedded from `templates/`
/// like the pipeline is, so the shipped copy and the repo copy cannot drift.
/// Paired as (path relative to the project root, body).
const TESTKIT_TEMPLATES: [(&str, &str); 6] = [
    ("testkit/__init__.py", include_str!("../../templates/testkit/__init__.py")),
    ("testkit/txscript_eval.py", include_str!("../../templates/testkit/txscript_eval.py")),
    (
        "testkit/test_txscript_eval.py",
        include_str!("../../templates/testkit/test_txscript_eval.py"),
    ),
    ("conftest.py", include_str!("../../templates/conftest.py")),
    ("pytest.ini", include_str!("../../templates/pytest.ini")),
    ("requirements-dev.txt", include_str!("../../templates/requirements-dev.txt")),
];
```

And the writer, next to `write_gitlab_ci`:

```rust
/// Write the Python test harness and its pytest wiring. Same scaffold contract
/// as every other template: each file is created when absent and replaced only
/// under `--force`, so a project that has evolved its own copy keeps it.
///
/// The harness's own self-tests ship with it on purpose: `pytest -q` with
/// nothing collected exits 5, which would make the pipeline's test job red on a
/// project that has not written any tests yet.
fn write_testkit(root: &Path, force: bool) -> Result<Vec<(&'static str, Scaffolded)>> {
    std::fs::create_dir_all(root.join("testkit"))
        .with_context(|| format!("creating {}", root.join("testkit").display()))?;
    TESTKIT_TEMPLATES
        .iter()
        .map(|(rel, body)| Ok((*rel, write_template_file(&root.join(rel), body, force)?)))
        .collect()
}
```

- [ ] **Step 4: Add them to the scaffold list**

`src/cli/init.rs:113-119` becomes:

```rust
    let mut scaffold: Vec<(&str, Scaffolded)> = vec![
        (".gitignore", write_gitignore(&cwd)?),
        (".gitattributes", write_gitattributes(&cwd)?),
        ("CLAUDE.md", write_claude_md(&cwd, force)?),
        ("README.md", write_readme(&cwd, &cfg, force)?),
        (".gitlab-ci.yml", write_gitlab_ci(&cwd, &cfg, force)?),
    ];
    scaffold.extend(write_testkit(&cwd, force)?);
```

The `--force` summary prints `{name:<16}`, which is narrower than
`testkit/test_txscript_eval.py`; widen it to `{name:<30}` at
`src/cli/init.rs:136` so the column stays aligned.

- [ ] **Step 5: Extend the `.gitignore` patterns and the agent guide**

In `write_gitignore`'s `PATTERNS` (`src/cli/init.rs:527-534`), add after `"/target"`:

```rust
        // pytest + CPython leave these beside the scaffolded testkit.
        "__pycache__/",
        "/.pytest_cache",
```

In `CLAUDE_MD_TEMPLATE`'s repo-layout block (`src/cli/init.rs:789`), add under the `.gitlab-ci.yml` line:

```
testkit/                                  formula/hook test harness (real txscript); `pytest -q`
requirements-dev.txt                      pinned pytest + txscript for the CI test job
```

- [ ] **Step 6: Extend `write_scaffold_files`**

Add to `write_scaffold_files` (`src/cli/init.rs:743-763`), after `write_gitlab_ci`:

```rust
    write_testkit(cwd, false)?;
```

and add the six paths to the `write_scaffold_files_writes_every_scaffold_file` unit test's expected list (`src/cli/init.rs:959-977`).

- [ ] **Step 7: Run the tests**

Run: `cargo test --test cli_init && cargo test --lib`
Expected: PASS.

- [ ] **Step 8: Commit**

```bash
git add src/cli/init.rs tests/cli_init.rs
git commit -m "feat(init): scaffold the formula testkit and its pytest wiring"
```

---

### Task 7: Prove the shipped testkit runs, and document the contract

**Files:**
- Modify: `tests/cli_init.rs` (add the pytest smoke test)
- Modify: `CLAUDE.md`, `README.md`
- Test: `tests/cli_init.rs`

**Interfaces:**
- Consumes: everything from Tasks 1–6.
- Produces: no new API.

- [ ] **Step 1: Write the test that runs the shipped testkit under pytest**

Add to `tests/cli_init.rs`:

```rust
/// The scaffolded harness must actually run. rdc's CI builds on tags and runs
/// no tests (`.github/workflows/`), so this guards `cargo test` on a developer
/// machine; it SKIPS rather than fails when the Python side isn't available,
/// which keeps `cargo test` green on a machine with no txscript installed.
#[test]
fn scaffolded_testkit_passes_its_own_pytest_suite() {
    let probe = std::process::Command::new("python3")
        .args(["-c", "import pytest, txscript"])
        .output();
    match probe {
        Ok(out) if out.status.success() => {}
        _ => {
            eprintln!(
                "SKIP scaffolded_testkit_passes_its_own_pytest_suite: \
                 python3 with pytest + txscript not available \
                 (pip install 'pytest>=8,<9' 'txscript==1.2.0')"
            );
            return;
        }
    }

    let dir = TempDir::new().unwrap();
    Command::cargo_bin("rdc")
        .unwrap()
        .current_dir(dir.path())
        .args(["init", "--env", "dev=https://example.rossum.app/api/v1:1"])
        .assert()
        .success();

    let out = std::process::Command::new("python3")
        .args(["-m", "pytest", "-q"])
        .current_dir(dir.path())
        .output()
        .expect("running pytest");
    assert!(
        out.status.success(),
        "the scaffolded testkit failed its own suite:\n{}\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );
    // Exit 0 rather than 5 is the whole point: a fresh project's test job is green.
    assert!(
        String::from_utf8_lossy(&out.stdout).contains("passed"),
        "expected collected tests, got: {}",
        String::from_utf8_lossy(&out.stdout)
    );
}
```

- [ ] **Step 2: Run it**

Run: `cargo test --test cli_init scaffolded_testkit -- --nocapture`
Expected: PASS with `17 passed` in the captured output, or the SKIP line if the local python lacks txscript. If it skips, install the deps and re-run once so the test is genuinely exercised: `python3 -m pip install --user 'pytest>=8,<9' 'txscript==1.2.0'`.

- [ ] **Step 3: Update `CLAUDE.md`**

In the "CI templates" section, replace the first bullet — "`templates/gitlab-ci.yml` is **embedded in the binary** with `include_str!` … `tests/cli_init.rs` compares the two byte-for-byte." — with:

```markdown
- `templates/gitlab-ci.yml` is **embedded in the binary** with `include_str!`
  (`src/cli/init.rs`) and written to `.gitlab-ci.yml` by `rdc init`. Edit the
  template, never a copy. Two `# >>> rdc:…` regions in it are **generated** from
  `rdc.toml` (`src/cli/gitlab_ci.rs`): the archive job's `parallel:matrix` and
  the drafted deploy jobs. So the test is in two halves — everything outside the
  markers must match the template byte-for-byte, and the committed region bodies
  must equal what `render_regions` produces for the canonical `dev`/`test`/`prod`
  example. Keep the committed example in step with the renderer, or
  `template_carries_both_regions` / the init test will say so.
- `rdc init` splices those regions on **every** run, including `--env`, and never
  touches a pipeline that has no markers. A markered file is spliced even under
  `--force`, so the static half of a project's pipeline is theirs once written;
  taking a newer binary's static half means deleting the file and re-initing.
- The Python testkit under `templates/testkit/` is embedded the same way and
  scaffolded alongside `conftest.py` / `pytest.ini` / `requirements-dev.txt`. It
  supports txscript **1.1.0 and 1.2.0** from one code path; `_unwrap` must test
  `isinstance(result, EvalResult)` and never `getattr(result, "value", result)`,
  because a formula returning a field hands back a proxy whose `.value` is the
  datapoint's raw string. Its self-tests ship on purpose: `pytest -q` with
  nothing collected exits 5 and turns the pipeline's test job red.
```

- [ ] **Step 4: Update `README.md`**

Replace the scaffold sentence at `README.md:108` with:

```markdown
Alongside the snapshot it scaffolds `CLAUDE.md`, `README.md`, `.gitignore`, `.gitattributes`, a `.gitlab-ci.yml` pipeline, and a Python test harness (`testkit/`, `conftest.py`, `pytest.ini`, `requirements-dev.txt`). The pipeline's archive matrix and its deploy buttons are generated from the envs in `rdc.toml` — one archive job per env, one drafted deploy button per env — inside two `# >>> rdc:…` marked regions that `rdc init` refreshes on every run, including when you add an env. Everything outside those markers is yours and is never touched; a pipeline with no markers at all is left completely alone. Fill in each draft's `RDC_SRC` before pressing it (it refuses to run until you do) and delete the drafts for envs you author by hand.

The harness evaluates a queue's formulas against the **real** txscript runtime, using that queue's own `schema.json`, so a `number` field is a number and a `date` field must be ISO:

```python
from testkit import evaluate_formula

FORMULAS = "envs/dev/workspaces/invoices/queues/cost-invoices/formulas"

def test_total_is_net_plus_tax():
    assert evaluate_formula(f"{FORMULAS}/amount_total.py", amount_net="100", amount_tax="21") == 121.0

def test_line_total_per_row():
    assert evaluate_formula(
        f"{FORMULAS}/item_total.py",
        rows={"line_items": [{"item_qty": "2", "item_price": "5"}]},
    ) == [10.0]
```

`pytest -q` runs it. Existing files are never touched — `rdc init --force` re-generates them from the current binary, and on its own (no `--env`) that is all it does.
```

Also update the `rdc init` row of the command table (`README.md:337`) to mention the testkit:

```markdown
| `rdc init` | Create a new project, or add an env to an existing one. Prompts interactively; regenerates the pipeline's env regions on every run; `--force` re-generates the scaffold files (`CLAUDE.md`, `README.md`, `.gitlab-ci.yml`, `testkit/`). |
```

- [ ] **Step 5: Run the whole suite**

Run: `cargo test`
Expected: PASS. `cargo clippy --all-targets -- -D warnings` should also be clean; do **not** run `cargo fmt` repo-wide (this repo is not fmt-clean under local rustfmt and the failure is pre-existing).

- [ ] **Step 6: Commit**

```bash
git add tests/cli_init.rs CLAUDE.md README.md
git commit -m "test(init): run the scaffolded testkit under pytest; document the region contract"
```

---

## Known consequence to confirm before Task 4

`write_gitlab_ci` splices a markered file **even under `--force`** (spec table, Section C). That keeps a project's hand-written jobs safe forever, but it also means a newer rdc's improvements to the *static* half of the template never reach an already-initialised project — the only route is deleting `.gitlab-ci.yml` and re-running `rdc init`. The alternative (markered + `--force` → wholesale regenerate, matching today's "`--force` means hand edits are lost") makes `--force` useful for picking up template improvements at the cost of discarding the user's own jobs. The spec chose the former; flag this to the user before implementing Task 4 if the trade-off should go the other way.
