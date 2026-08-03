# Desktop multi-env — Phase 1 (foundation) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make the desktop app understand multi-environment rdc projects — one project (folder) exposing N environments — and view/pull-sync each env independently, replacing the hardcoded single `main` env.

**Architecture:** The `rdc` core is unchanged. In the bridge crate (`desktop/rust`), `discover` stops requiring `[envs.main]` and enumerates every env; the FRB surface returns a nested `ProjectSummary { envs: Vec<EnvSummary> }` and gains a per-env `sync_env`. In Flutter, `AppState` models a project with envs, keys transient state by `(folder, env)`, and the sidebar becomes a two-level project → env tree. This is the first of four phases from the design spec (`docs/superpowers/specs/2026-08-03-desktop-multi-env-and-promote-design.md`); env management, the Project view, and promote follow in later plans.

**Tech Stack:** Rust (bridge crate `rdc_bridge`, path-dep on `rdc`), `flutter_rust_bridge` 2.12.0 (committed generated glue), Flutter/Dart (Material), golden + integration tests.

## Global Constraints

- **CLI core is NOT modified** — only `desktop/`. Copy no customer names/identifiers into code, tests, fixtures, or commit messages; use neutral placeholders (`acme`, `main`, `test`, `prod`).
- **On-disk format is unchanged:** `rdc.toml [envs.<name>]`, `envs/<name>/`, `secrets/<name>.secrets.json`, `.rdc/state/<name>.lock.json`. The CLI and app keep sharing the same folders.
- **Env-level sync stays pull-only** in this app (embed `sync_no_push_logged`); no remote writes land in Phase 1.
- **Regenerating FRB bindings** (after any Rust `api` change): `cargo install flutter_rust_bridge_codegen --version 2.12.0` (once), then `cd desktop && flutter_rust_bridge_codegen generate`. Commit the regenerated `desktop/lib/src/rust/**` and `desktop/rust/src/frb_generated.rs`.
- **The bridge crate is its own cargo workspace** (`desktop/rust`), separate from the parent (parent sets `panic = "abort"`). Run its tests with `cd desktop/rust && cargo test`.
- **Baseline before starting:** `cd desktop/rust && cargo test` green; `cd desktop && flutter analyze` clean; `flutter test test/golden_mdh_test.dart` green. Record failures before changing anything.

---

### Task 1: `discover` enumerates all environments

**Files:**
- Modify: `desktop/rust/src/discover.rs` (replace the `Connection`/`main`-only model with a project+envs model)

**Interfaces:**
- Produces (consumed by Task 2):
  - `struct EnvInfo { pub name: String, pub api_base: String, pub org_id: u64, pub auth_kind: AuthKindRaw, pub last_sync_unix: Option<i64>, pub file_count: u64 }`
  - `struct Project { pub folder: PathBuf, pub envs: Vec<EnvInfo> }` with `fn name(&self) -> &str` (folder basename) and `fn id(&self) -> &str` (== name)
  - `fn scan(parent: &Path) -> Vec<Project>`, `fn find(parent: &Path, name: &str) -> Option<Project>`, `fn inspect(folder: &Path) -> Option<Project>`, `fn count_files(p: &Path) -> u64` (unchanged), `enum AuthKindRaw { Token, Password }` (unchanged)

- [ ] **Step 1: Write the failing tests**

Replace the existing `#[cfg(test)] mod tests` block in `desktop/rust/src/discover.rs` with these (the `seed_connection` helper gains an env name):

```rust
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
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cd desktop/rust && cargo test discover`
Expected: FAIL to compile (`Project`, `EnvInfo`, new `inspect` shape don't exist yet).

- [ ] **Step 3: Rewrite the module body**

Replace everything in `desktop/rust/src/discover.rs` **above** the `#[cfg(test)]` block with:

```rust
//! Project discovery via directory scan. A Project is any folder under the
//! caller-supplied parent that looks like an rdc project: an `rdc.toml` with at
//! least one `[envs.<name>]` section. Each env is surfaced independently. All
//! state is derived from on-disk artifacts — there is no registry.

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
            let last_sync_unix =
                std::fs::metadata(folder.join(format!(".rdc/state/{name}.lock.json")))
                    .ok()
                    .and_then(|m| m.modified().ok())
                    .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                    .and_then(|d| i64::try_from(d.as_secs()).ok());
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
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cd desktop/rust && cargo test discover`
Expected: PASS (5 tests). The crate won't fully build yet because `api/rdc.rs` still references the old `Connection` API — that's fixed in Task 2. If `cargo test discover` fails to build due to `api/rdc.rs`, temporarily run only the module: `cargo test --lib discover:: 2>&1 | head` and confirm the discover tests compile; proceed to Task 2 to restore a full build.

- [ ] **Step 5: Commit**

```bash
git add desktop/rust/src/discover.rs
git commit -m "feat(desktop): discover enumerates every env, not just main"
```

---

### Task 2: Bridge surface — `ProjectSummary`/`EnvSummary`, `list_projects`, `validate`, `sync_env`, project add/edit/trash

**Files:**
- Modify: `desktop/rust/src/api/rdc.rs` (types + all fns)
- Regenerate: `desktop/lib/src/rust/**`, `desktop/rust/src/frb_generated.rs`
- Test: `desktop/integration_test/bridge_test.dart` (updated in Task 5; here we keep the Rust side compiling + a Rust unit test)

**Interfaces:**
- Consumes: `discover::{Project, EnvInfo, AuthKindRaw, scan, find, inspect, count_files}` (Task 1)
- Produces (consumed by Tasks 3–5, Dart names in parentheses):
  - `struct EnvSummary { name: String, api_base: String, org_id: u64, auth_kind: AuthKind, last_sync_unix: Option<i64>, file_count: u64 }`
  - `struct ProjectSummary { id: String, name: String, folder: String, envs: Vec<EnvSummary> }`
  - `fn list_projects(parent: String) -> Vec<ProjectSummary>` (`listProjects`)
  - `fn validate_existing_project(path: String) -> Result<ProjectSummary>` (`validateExistingProject`)
  - `fn add_project(parent: String, input: AddConnectionInput) -> Result<ProjectSummary>` (`addProject`) — creates the project + first env named `main`
  - `fn edit_project(folder: String, env: String, input: EditConnectionInput) -> Result<ProjectSummary>` (`editProject`) — renames the project folder from `input.name` and rewrites `env`'s api_base/org_id/credentials
  - `fn sync_env(folder: String, env: String, api_base: String, org_id: u64, sink: StreamSink<SyncPhase>) -> Result<()>` (`syncEnv`) — pull-only
  - `fn trash_project(folder: String) -> Result<()>` (`trashProject`), `reveal_in_file_manager` (unchanged), `rdc_version` (unchanged)
  - `AddConnectionInput` / `EditConnectionInput` / `AuthKind` / `SyncPhase` structs unchanged in shape

- [ ] **Step 1: Write a failing Rust unit test**

Append to `desktop/rust/src/api/rdc.rs` (inside a new `#[cfg(test)] mod tests`):

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn project_summary_from_multi_env_project() {
        let tmp = tempfile::tempdir().unwrap();
        let folder = tmp.path().join("acme");
        std::fs::create_dir_all(&folder).unwrap();
        std::fs::write(
            folder.join("rdc.toml"),
            "[envs.dev]\napi_base = \"https://d.test/api/v1\"\norg_id = 1\n\
             [envs.prod]\napi_base = \"https://p.test/api/v1\"\norg_id = 2\n",
        )
        .unwrap();
        let p = discover::inspect(&folder).unwrap();
        let summary = ProjectSummary::from(&p);
        assert_eq!(summary.name, "acme");
        assert_eq!(summary.envs.len(), 2);
        assert_eq!(summary.envs[0].name, "dev");
        assert_eq!(summary.envs[1].org_id, 2);
    }
}
```

- [ ] **Step 2: Run it to verify it fails**

Run: `cd desktop/rust && cargo test project_summary_from_multi_env_project`
Expected: FAIL to compile (`ProjectSummary`, `EnvSummary`, `From<&Project>` don't exist).

- [ ] **Step 3: Rewrite the bridge types + list/validate**

In `desktop/rust/src/api/rdc.rs`, replace the `ConnectionSummary` struct and its `From<&Connection>` impl with:

```rust
#[derive(Debug, Clone)]
pub struct EnvSummary {
    pub name: String,
    pub api_base: String,
    pub org_id: u64,
    pub auth_kind: AuthKind,
    pub last_sync_unix: Option<i64>,
    pub file_count: u64,
}

impl From<&crate::discover::EnvInfo> for EnvSummary {
    fn from(e: &crate::discover::EnvInfo) -> Self {
        Self {
            name: e.name.clone(),
            api_base: e.api_base.clone(),
            org_id: e.org_id,
            auth_kind: e.auth_kind.into(),
            last_sync_unix: e.last_sync_unix,
            file_count: e.file_count,
        }
    }
}

#[derive(Debug, Clone)]
pub struct ProjectSummary {
    pub id: String,
    pub name: String,
    pub folder: String,
    pub envs: Vec<EnvSummary>,
}

impl From<&Project> for ProjectSummary {
    fn from(p: &Project) -> Self {
        Self {
            id: p.id().to_string(),
            name: p.name().to_string(),
            folder: p.folder.display().to_string(),
            envs: p.envs.iter().map(EnvSummary::from).collect(),
        }
    }
}
```

Update the import line `use crate::discover::{self, AuthKindRaw, Connection};` to `use crate::discover::{self, AuthKindRaw, Project};`.

Replace `list_connections` with:

```rust
/// List every Project under `parent`. Non-project folders are skipped.
pub fn list_projects(parent: String) -> Vec<ProjectSummary> {
    discover::scan(Path::new(&parent))
        .iter()
        .map(ProjectSummary::from)
        .collect()
}
```

Replace `validate_existing_project` with (drops the `[envs.main]` requirement):

```rust
/// Validate that `path` is an rdc project (≥1 env, any names) and return its
/// summary. Does not move, copy, or symlink anything.
pub fn validate_existing_project(path: String) -> Result<ProjectSummary> {
    let source = PathBuf::from(&path);
    if !source.is_dir() {
        return Err(anyhow!("Not a folder: {path}"));
    }
    if !source.join("rdc.toml").exists() {
        return Err(anyhow!(
            "{path} doesn't look like an rdc project (no rdc.toml). Run `rdc init` there first."
        ));
    }
    discover::inspect(&source)
        .as_ref()
        .map(ProjectSummary::from)
        .ok_or_else(|| anyhow!("{path} has no environments defined in rdc.toml."))
}
```

- [ ] **Step 4: Rewrite add/edit/sync/trash for the project+env model**

Replace `add_connection` with `add_project` (first env is `main`; body otherwise identical to today):

```rust
/// Create a new Project: write `rdc.toml` + secrets for a first env named
/// `main` under a unique slug. (Additional envs are added in a later phase.)
pub fn add_project(parent: String, input: AddConnectionInput) -> Result<ProjectSummary> {
    let parent = PathBuf::from(parent);
    let used: HashSet<String> = discover::scan(&parent)
        .iter()
        .map(|p| p.name().to_string())
        .collect();
    let slug = rdc::slug::slugify_unique(&input.name, &used);
    let folder = parent.join(&slug);
    std::fs::create_dir_all(&folder).map_err(|e| anyhow!("creating folder: {e}"))?;

    let api_base = input.api_base.trim_end_matches('/').to_string();
    let rdc_toml = format!(
        "[envs.main]\napi_base = \"{api_base}\"\norg_id = {}\n",
        input.org_id
    );
    std::fs::write(folder.join("rdc.toml"), rdc_toml)
        .map_err(|e| anyhow!("writing rdc.toml: {e}"))?;

    write_credentials(
        &folder, "main",
        input.auth_kind,
        input.token.as_deref(),
        input.username.as_deref(),
        input.password.as_deref(),
    )?;

    discover::find(&parent, &slug)
        .as_ref()
        .map(ProjectSummary::from)
        .ok_or_else(|| anyhow!("Project not found after add"))
}
```

Replace `edit_connection` with `edit_project(folder, env, input)` — same behavior as today but env-parameterized (rename the folder from `input.name`; rewrite `env`'s config + credentials):

```rust
/// Rename the project folder (from `input.name`) if it changed, and rewrite the
/// named `env`'s api_base/org_id through rdc's own config writer. Credentials
/// are only replaced when supplied (blank = keep existing).
pub fn edit_project(
    folder: String,
    env: String,
    input: EditConnectionInput,
) -> Result<ProjectSummary> {
    let mut folder = PathBuf::from(&folder);
    if !folder.join("rdc.toml").exists() {
        return Err(anyhow!("Project not found"));
    }
    let parent = folder
        .parent()
        .ok_or_else(|| anyhow!("Project has no parent folder"))?
        .to_path_buf();
    let current_slug = folder
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or_default()
        .to_string();
    let used: HashSet<String> = discover::scan(&parent)
        .iter()
        .map(|p| p.name().to_string())
        .filter(|n| n != &current_slug)
        .collect();
    let desired_slug = rdc::slug::slugify_unique(&input.name, &used);
    if desired_slug != current_slug {
        let new_folder = parent.join(&desired_slug);
        if new_folder.exists() {
            return Err(anyhow!("A project named \"{}\" already exists here.", input.name));
        }
        std::fs::rename(&folder, &new_folder).map_err(|e| anyhow!("renaming the project: {e}"))?;
        folder = new_folder;
    }

    let toml_path = folder.join("rdc.toml");
    let mut cfg = rdc::config::ProjectConfig::load(&toml_path).map_err(|e| anyhow!("{e:#}"))?;
    let ec = cfg
        .envs
        .get_mut(&env)
        .ok_or_else(|| anyhow!("This project has no `{env}` environment."))?;
    ec.api_base = input.api_base.trim_end_matches('/').to_string();
    ec.org_id = input.org_id;
    cfg.save(&toml_path).map_err(|e| anyhow!("{e:#}"))?;

    let has_new_credentials = match input.auth_kind {
        AuthKind::Token => input.token.as_deref().is_some_and(|s| !s.is_empty()),
        AuthKind::Password => {
            input.username.as_deref().is_some_and(|s| !s.is_empty())
                || input.password.as_deref().is_some_and(|s| !s.is_empty())
        }
    };
    if has_new_credentials {
        let _ = std::fs::remove_file(folder.join(format!("secrets/{env}.secrets.json")));
        write_credentials(
            &folder, &env,
            input.auth_kind,
            input.token.as_deref(),
            input.username.as_deref(),
            input.password.as_deref(),
        )?;
    }

    discover::inspect(&folder)
        .as_ref()
        .map(ProjectSummary::from)
        .ok_or_else(|| anyhow!("Project not found after edit"))
}
```

Change `write_credentials` to take an `env: &str` parameter (replace the two hardcoded `"main"` literals with `env`):

```rust
fn write_credentials(
    folder: &Path,
    env: &str,
    auth: AuthKind,
    token: Option<&str>,
    username: Option<&str>,
    password: Option<&str>,
) -> Result<()> {
    match auth {
        AuthKind::Token => {
            let t = token.filter(|s| !s.is_empty()).ok_or_else(|| anyhow!("Token is required."))?;
            rdc::secrets::write_secrets_file(folder, env, t, None).map_err(|e| anyhow!("{e:#}"))?;
        }
        AuthKind::Password => {
            let u = username.filter(|s| !s.is_empty()).ok_or_else(|| anyhow!("Username is required."))?;
            let p = password.filter(|s| !s.is_empty()).ok_or_else(|| anyhow!("Password is required."))?;
            rdc::secrets::save_password_credentials(folder, env, u, p).map_err(|e| anyhow!("{e:#}"))?;
        }
    }
    Ok(())
}
```

Replace `sync_connection` with `sync_env` (add the `env` param, replace every `"main"` literal with `env`, and the completion `count_files` path with `envs/<env>`):

```rust
/// Pull-only sync of one environment. Scaffolds init files, resolves the token
/// (silent re-login in password mode), then runs `sync_no_push`. Progress is
/// streamed as `SyncPhase`.
pub fn sync_env(
    folder: String,
    env: String,
    api_base: String,
    org_id: u64,
    sink: StreamSink<SyncPhase>,
) -> Result<()> {
    let folder = PathBuf::from(folder);
    let _ = sink.add(SyncPhase::Started);

    let forwarder = LineForwarder { sink: sink.clone(), buf: Vec::new() };
    let result: Result<u64> = block_on(async {
        rdc::cli::init::write_scaffold_files(&folder, &env, &api_base, org_id)?;
        let token = rdc::secrets::resolve_token(&folder, &env, &api_base).await?;
        rdc::cli::sync::embed::sync_no_push_logged(&folder, &env, &token, Box::new(forwarder)).await?;
        Ok(discover::count_files(&folder.join(format!("envs/{env}"))))
    });

    match result {
        Ok(file_count) => {
            let _ = sink.add(SyncPhase::Log { line: format!("✓ done · {file_count} files") });
            let _ = sink.add(SyncPhase::Done { file_count });
        }
        Err(e) => {
            let _ = sink.add(SyncPhase::Error { message: format!("{e:#}") });
        }
    }
    Ok(())
}
```

Rename `trash_connection` → `trash_project` (body unchanged). Leave `reveal_in_file_manager`, `rdc_version`, `LineForwarder`, `block_on`, `AddConnectionInput`, `EditConnectionInput`, `AuthKind`, `SyncPhase` as they are.

- [ ] **Step 5: Verify the Rust unit test passes**

Run: `cd desktop/rust && cargo test`
Expected: PASS (discover tests + `project_summary_from_multi_env_project`). Warnings about unused Dart-facing fns are fine.

- [ ] **Step 6: Regenerate FRB bindings**

Run:
```bash
cargo install flutter_rust_bridge_codegen --version 2.12.0   # once, if not present
cd desktop && flutter_rust_bridge_codegen generate
```
Expected: `desktop/lib/src/rust/api/rdc.dart` now exports `listProjects`, `validateExistingProject`, `addProject`, `editProject`, `syncEnv`, `trashProject`, and the `ProjectSummary`/`EnvSummary` classes; `sync_connection`/`list_connections`/etc. are gone.

- [ ] **Step 7: Commit**

```bash
git add desktop/rust/src/api/rdc.rs desktop/lib/src/rust desktop/rust/src/frb_generated.rs
git commit -m "feat(desktop): bridge exposes projects with envs + per-env sync_env"
```

---

### Task 3: `AppState` models a project with envs, keyed by (folder, env)

**Files:**
- Modify: `desktop/lib/src/app_state.dart` (rewrite model + selection + keying + sync)
- Modify: `desktop/lib/src/dialogs.dart` (rename types/calls; edit now targets the selected env)

**Interfaces:**
- Consumes: `listProjects`, `validateExistingProject`, `addProject`, `editProject`, `syncEnv`, `trashProject`, `ProjectSummary`, `EnvSummary`, `AddConnectionInput`, `EditConnectionInput` (Task 2)
- Produces (consumed by Tasks 4–5):
  - `class ProjectItem { final ProjectSummary summary; final bool isExternal; }`
  - `AppState` fields: `List<ProjectItem> projects`, `String? selectedFolder`, `String? selectedEnv`
  - `ProjectItem? get selected`, `EnvSummary? get selectedEnvSummary`
  - `String envKey(String folder, String env)` (== `'$folder\u0000$env'`) and maps `syncState/syncMessage/syncLog` keyed by it
  - `void selectProject(String folder)` (selects the project's first env), `void selectEnv(String folder, String env)`
  - `void syncEnvItem(ProjectItem p, EnvSummary e)`
  - Async: `addProjectEntry`, `editEnvEntry(ProjectItem, EnvSummary, EditConnectionInput)`, `openExisting`, `removeOrDetach`, `reveal`, `reload`, `setParentFolder`, `clearError`

- [ ] **Step 1: Write the failing test**

Create `desktop/test/app_state_test.dart`:

```dart
import 'package:desktop/src/app_state.dart';
import 'package:desktop/src/rust/api/rdc.dart';
import 'package:desktop/src/settings.dart';
import 'package:flutter_test/flutter_test.dart';

ProjectItem _p(String folder, List<EnvSummary> envs, {bool ext = false}) => ProjectItem(
      ProjectSummary(id: folder, name: folder.split('/').last, folder: folder, envs: envs),
      ext,
    );

EnvSummary _e(String name, int org) => EnvSummary(
      name: name, apiBase: 'https://x.test/api/v1', orgId: BigInt.from(org),
      authKind: AuthKind.token, lastSyncUnix: null, fileCount: BigInt.zero,
    );

void main() {
  test('selecting a project picks its first env; selecting an env pins it', () {
    final s = AppState(Settings(parentFolder: '/tmp'));
    s.projects = [_p('/tmp/acme', [_e('dev', 1), _e('prod', 2)])];
    s.selectProject('/tmp/acme');
    expect(s.selected!.summary.folder, '/tmp/acme');
    expect(s.selectedEnv, 'dev');
    expect(s.selectedEnvSummary!.orgId, BigInt.from(1));
    s.selectEnv('/tmp/acme', 'prod');
    expect(s.selectedEnvSummary!.orgId, BigInt.from(2));
  });

  test('sync state is keyed per (folder, env)', () {
    final s = AppState(Settings(parentFolder: '/tmp'));
    s.syncState[s.envKey('/tmp/acme', 'dev')] = SyncState.running;
    expect(s.syncState[s.envKey('/tmp/acme', 'prod')], isNull);
    expect(s.syncState[s.envKey('/tmp/acme', 'dev')], SyncState.running);
  });
}
```

- [ ] **Step 2: Run it to verify it fails**

Run: `cd desktop && flutter test test/app_state_test.dart`
Expected: FAIL to compile (`ProjectItem`, `AppState.projects`, `selectProject`, `envKey`, `selectedEnvSummary` don't exist).

- [ ] **Step 3: Rewrite `app_state.dart`**

Replace the whole file with:

```dart
import 'package:flutter/foundation.dart';

import 'error_text.dart';
import 'rust/api/rdc.dart';
import 'settings.dart';

enum SyncState { idle, running, done, error }

/// A discovered project plus whether it lives outside the parent folder (an
/// "external" project attached via Open Existing).
class ProjectItem {
  final ProjectSummary summary;
  final bool isExternal;
  const ProjectItem(this.summary, this.isExternal);
}

/// Single source of truth for the UI. Wraps the Rust bridge and derives the
/// project list from disk (parent scan ∪ attached externals). Transient sync
/// state is keyed per (folder, env) via [envKey].
class AppState extends ChangeNotifier {
  AppState(this._settings);

  final Settings _settings;

  List<ProjectItem> projects = [];
  String? selectedFolder;
  String? selectedEnv;
  bool loading = false;
  String? lastError;

  final Map<String, SyncState> syncState = {};
  final Map<String, String> syncMessage = {};
  final Map<String, List<String>> syncLog = {};

  String? get parentFolder => _settings.parentFolder;

  String envKey(String folder, String env) => '$folder\u0000$env';

  ProjectItem? get selected {
    final folder = selectedFolder;
    if (folder == null) return null;
    for (final p in projects) {
      if (p.summary.folder == folder) return p;
    }
    return null;
  }

  EnvSummary? get selectedEnvSummary {
    final p = selected;
    final env = selectedEnv;
    if (p == null || env == null) return null;
    for (final e in p.summary.envs) {
      if (e.name == env) return e;
    }
    return null;
  }

  void selectProject(String folder) {
    selectedFolder = folder;
    ProjectItem? p;
    for (final it in projects) {
      if (it.summary.folder == folder) p = it;
    }
    selectedEnv = (p != null && p.summary.envs.isNotEmpty) ? p.summary.envs.first.name : null;
    notifyListeners();
  }

  void selectEnv(String folder, String env) {
    selectedFolder = folder;
    selectedEnv = env;
    notifyListeners();
  }

  Future<void> setParentFolder(String path) async {
    _settings.parentFolder = path;
    _settings.save();
    await reload();
  }

  Future<void> reload() async {
    loading = true;
    notifyListeners();

    final byFolder = <String, ProjectItem>{};
    try {
      final parent = _settings.parentFolder;
      if (parent != null) {
        for (final s in await listProjects(parent: parent)) {
          byFolder[s.folder] = ProjectItem(s, false);
        }
      }
      final stillValid = <String>[];
      for (final path in List<String>.from(_settings.externalPaths)) {
        try {
          final s = await validateExistingProject(path: path);
          byFolder.putIfAbsent(s.folder, () => ProjectItem(s, true));
          stillValid.add(path);
        } catch (_) {}
      }
      if (stillValid.length != _settings.externalPaths.length) {
        _settings.externalPaths = stillValid;
        _settings.save();
      }
    } catch (e) {
      lastError = errorText(e);
    }

    projects = byFolder.values.toList()
      ..sort((a, b) => a.summary.name.toLowerCase().compareTo(b.summary.name.toLowerCase()));
    if (selectedFolder != null && !projects.any((p) => p.summary.folder == selectedFolder)) {
      selectedFolder = null;
      selectedEnv = null;
    }
    // Re-pin the selected env if it vanished (e.g. removed on disk).
    if (selectedFolder != null && selected != null &&
        !selected!.summary.envs.any((e) => e.name == selectedEnv)) {
      selectedEnv = selected!.summary.envs.isNotEmpty ? selected!.summary.envs.first.name : null;
    }
    if (selectedFolder == null && projects.isNotEmpty) {
      selectProject(projects.first.summary.folder);
    }
    loading = false;
    notifyListeners();
  }

  Future<void> addProjectEntry(AddConnectionInput input) async {
    final parent = _settings.parentFolder;
    if (parent == null) throw Exception('Choose a parent folder first.');
    final s = await addProject(parent: parent, input: input);
    await reload();
    selectProject(s.folder);
  }

  Future<void> editEnvEntry(ProjectItem item, EnvSummary env, EditConnectionInput input) async {
    final updated = await editProject(folder: item.summary.folder, env: env.name, input: input);
    if (item.isExternal && updated.folder != item.summary.folder) {
      final i = _settings.externalPaths.indexOf(item.summary.folder);
      if (i >= 0) {
        _settings.externalPaths[i] = updated.folder;
        _settings.save();
      }
    }
    await reload();
    selectEnv(updated.folder, env.name);
  }

  Future<void> openExisting(String path) async {
    final s = await validateExistingProject(path: path);
    final parent = _settings.parentFolder;
    final underParent = parent != null && s.folder.startsWith(parent);
    if (!underParent && !_settings.externalPaths.contains(s.folder)) {
      _settings.externalPaths.add(s.folder);
      _settings.save();
    }
    await reload();
    selectProject(s.folder);
  }

  Future<void> removeOrDetach(ProjectItem item) async {
    if (item.isExternal) {
      _settings.externalPaths.remove(item.summary.folder);
      _settings.save();
    } else {
      await trashProject(folder: item.summary.folder);
    }
    if (selectedFolder == item.summary.folder) {
      selectedFolder = null;
      selectedEnv = null;
    }
    await reload();
  }

  Future<void> reveal(String folder) => revealInFileManager(path: folder);

  void syncEnvItem(ProjectItem item, EnvSummary env) {
    final folder = item.summary.folder;
    final k = envKey(folder, env.name);
    syncState[k] = SyncState.running;
    syncMessage.remove(k);
    syncLog[k] = <String>[];
    notifyListeners();

    syncEnv(folder: folder, env: env.name, apiBase: env.apiBase, orgId: env.orgId).listen(
      (phase) {
        switch (phase) {
          case SyncPhase_Started():
            syncState[k] = SyncState.running;
          case SyncPhase_Log(:final line):
            (syncLog[k] ??= <String>[]).add(line);
          case SyncPhase_Done(:final fileCount):
            syncState[k] = SyncState.done;
            syncMessage[k] = 'Pulled $fileCount files';
            reload();
          case SyncPhase_Error(:final message):
            syncState[k] = SyncState.error;
            syncMessage[k] = message;
        }
        notifyListeners();
      },
      onError: (Object e) {
        syncState[k] = SyncState.error;
        syncMessage[k] = errorText(e);
        notifyListeners();
      },
    );
  }

  void clearError() {
    lastError = null;
    notifyListeners();
  }
}
```

- [ ] **Step 4: Update `dialogs.dart` to the new names + per-env edit**

In `desktop/lib/src/dialogs.dart`:
- `AddConnectionDialog._submit`: change `widget.state.addConnectionEntry(` → `widget.state.addProjectEntry(`.
- `EditConnectionDialog`: change the field `final ConnItem item;` → `final ProjectItem item;` and add `final EnvSummary env;` to the widget + its constructor. Seed controllers from `widget.env` instead of `widget.item.summary`:
  - `_apiBase` text ← `widget.env.apiBase`, `_orgId` text ← `widget.env.orgId.toString()`, `_auth` ← `widget.env.authKind`, `_name` text ← `widget.item.summary.name`.
- `EditConnectionDialog._submit`: change `widget.state.editConnectionEntry(widget.item, ...)` → `widget.state.editEnvEntry(widget.item, widget.env, ...)`.
- `RemoveDialog`: change `final ConnItem item;` → `final ProjectItem item;` (field access `item.summary.name`/`item.isExternal` is unchanged).

- [ ] **Step 5: Run the AppState test to verify it passes**

Run: `cd desktop && flutter test test/app_state_test.dart`
Expected: PASS (2 tests). `flutter analyze` will still flag `home_page.dart` — fixed in Task 4.

- [ ] **Step 6: Commit**

```bash
git add desktop/lib/src/app_state.dart desktop/lib/src/dialogs.dart desktop/test/app_state_test.dart
git commit -m "feat(desktop): AppState models projects+envs, sync keyed per (folder,env)"
```

---

### Task 4: Two-level sidebar + per-env selection and sync in the UI

**Files:**
- Modify: `desktop/lib/src/home_page.dart` (sidebar tree, env-scoped panes, per-env sync wiring)

**Interfaces:**
- Consumes: `AppState` (Task 3): `projects`, `selected`, `selectedEnvSummary`, `selectedEnv`, `selectProject`, `selectEnv`, `syncEnvItem`, `envKey`, `ProjectItem`, `EnvSummary`
- Produces: an app that compiles (`flutter analyze` clean) and renders a project → env sidebar; selecting an env shows its Overview/Log/Files and a Sync button that pull-syncs that env.

Because `home_page.dart` is large and every `item.summary.{apiBase,orgId,authKind,lastSyncUnix,fileCount}` reference now comes from the **selected env**, the change is mechanical but wide. Follow these concrete edits; the `flutter analyze`/widget-test gates at Steps 6–7 catch anything missed.

- [ ] **Step 1: Write the failing widget test**

Create `desktop/test/sidebar_multi_env_test.dart`:

```dart
import 'package:desktop/src/app_state.dart';
import 'package:desktop/src/home_page.dart';
import 'package:desktop/src/mdh_theme.dart';
import 'package:desktop/src/rust/api/rdc.dart';
import 'package:desktop/src/settings.dart';
import 'package:flutter/material.dart';
import 'package:flutter_test/flutter_test.dart';

EnvSummary _e(String name, int org) => EnvSummary(
      name: name, apiBase: 'https://x.test/api/v1', orgId: BigInt.from(org),
      authKind: AuthKind.token, lastSyncUnix: 1000, fileCount: BigInt.from(5),
    );

AppState _seeded() {
  final s = AppState(Settings(parentFolder: '/tmp'));
  s.projects = [
    ProjectItem(ProjectSummary(id: 'acme', name: 'acme', folder: '/tmp/acme',
        envs: [_e('dev', 1), _e('prod', 2)]), false),
  ];
  s.selectProject('/tmp/acme');
  return s;
}

void main() {
  testWidgets('sidebar shows env children and selecting one pins it', (t) async {
    final s = _seeded();
    await t.pumpWidget(MaterialApp(
      theme: mdhTheme(Brightness.light),
      home: Scaffold(body: MdhScaffold(
        state: s,
        view: NavView.connection,
        onSelectEnv: (folder, env) => s.selectEnv(folder, env),
      )),
    ));
    await t.pumpAndSettle();
    expect(find.text('dev'), findsOneWidget);
    expect(find.text('prod'), findsOneWidget);
    await t.tap(find.text('prod'));
    await t.pumpAndSettle();
    expect(s.selectedEnv, 'prod');
  });
}
```

- [ ] **Step 2: Run it to verify it fails**

Run: `cd desktop && flutter test test/sidebar_multi_env_test.dart`
Expected: FAIL to compile (`MdhScaffold` has no `onSelectEnv`; sidebar has no env rows).

- [ ] **Step 3: Update the top-of-file helpers and callbacks**

In `home_page.dart`:
- `_statusOf(AppState s, ConnItem it)` → `_statusOf(AppState s, ProjectItem it, EnvSummary env)`; change the map lookup to `s.syncState[s.envKey(it.summary.folder, env.name)]` and the `lastSyncUnix` read to `env.lastSyncUnix`.
- In `_HomePageState`: add `onSelectEnv: (folder, env) => setState(() { state.selectEnv(folder, env); _view = NavView.connection; })` to the `MdhScaffold(...)` call, and change `onSelectConn` to call `state.selectProject(folder)`. Change `onSync: (i) => state.sync(i)` → `onSync: (p, e) => state.syncEnvItem(p, e)`. Change `_syncAll` to iterate envs: `for (final p in state.projects) { for (final e in p.summary.envs) state.syncEnvItem(p, e); }`. Update `_editConnection`/`_reveal`/`_confirmRemove` parameter types from `ConnItem` to `ProjectItem`.

- [ ] **Step 4: Update `MdhScaffold` + `_Sidebar` to a two-level tree**

- `MdhScaffold`: add `final void Function(String folder, String env)? onSelectEnv;` (+ constructor param). Change `onSync`/`onEdit`/`onReveal`/`onRemove` callback types that took `ConnItem` to take `ProjectItem` (and `onSync` to `void Function(ProjectItem, EnvSummary)`). Pass `onSelectEnv` into `_Sidebar` and `_ConnMain`.
- `_Sidebar`: rename the label `'CONNECTIONS'` → `'PROJECTS'` and the tooltip `'New connection'` → `'New project'`. Replace the `_SidebarRow` list with a project tree: for each `ProjectItem p` render a `_ProjectRow` (label = `p.summary.name`; tapping calls `onSelect(p.summary.folder)` which selects the project + first env) followed by, for each `EnvSummary e in p.summary.envs`, an indented `_EnvRow` (label = `e.name`, a status dot from `_statusOf(state, p, e)`, selected when `state.selectedFolder == p.summary.folder && state.selectedEnv == e.name`; tap calls `onSelectEnv(p.summary.folder, e.name)`).

Add these two widgets (adapting `_SidebarRow`'s styling):

```dart
class _ProjectRow extends StatelessWidget {
  const _ProjectRow({required this.state, required this.item, required this.onSelect});
  final AppState state;
  final ProjectItem item;
  final void Function(String) onSelect;
  @override
  Widget build(BuildContext context) {
    final c = MdhColors.of(context);
    final sel = item.summary.folder == state.selectedFolder;
    return Padding(
      padding: const EdgeInsets.only(top: 4, bottom: 1),
      child: InkWell(
        onTap: () => onSelect(item.summary.folder),
        mouseCursor: SystemMouseCursors.click,
        borderRadius: BorderRadius.circular(6),
        child: Padding(
          padding: const EdgeInsets.symmetric(horizontal: 10, vertical: 6),
          child: Row(children: [
            Icon(Icons.folder_outlined, size: 15, color: sel ? c.accent : c.textSecondary),
            const SizedBox(width: 8),
            Expanded(child: Text(item.summary.name,
                overflow: TextOverflow.ellipsis,
                style: TextStyle(color: c.textPrimary, fontSize: 13, fontWeight: FontWeight.w600))),
            if (item.isExternal) Text('ext', style: _mono(c.textHint, 10)),
          ]),
        ),
      ),
    );
  }
}

class _EnvRow extends StatelessWidget {
  const _EnvRow({required this.state, required this.item, required this.env, required this.onSelect});
  final AppState state;
  final ProjectItem item;
  final EnvSummary env;
  final void Function(String folder, String env) onSelect;
  @override
  Widget build(BuildContext context) {
    final c = MdhColors.of(context);
    final sel = item.summary.folder == state.selectedFolder && env.name == state.selectedEnv;
    final st = _statusOf(state, item, env);
    final dotColor = switch (st) { _St.error => c.danger, _St.never => c.textHint, _ => c.successFg };
    final sub = switch (st) {
      _St.running => 'syncing…', _St.error => 'failed',
      _St.synced => _rel(env.lastSyncUnix), _St.never => 'never',
    };
    return Padding(
      padding: const EdgeInsets.only(left: 14, top: 1, bottom: 1),
      child: InkWell(
        onTap: () => onSelect(item.summary.folder, env.name),
        mouseCursor: SystemMouseCursors.click,
        borderRadius: BorderRadius.circular(6),
        child: Container(
          padding: const EdgeInsets.symmetric(horizontal: 10, vertical: 6),
          decoration: BoxDecoration(
            color: sel ? c.accent : Colors.transparent, borderRadius: BorderRadius.circular(6)),
          child: Row(children: [
            Container(width: 7, height: 7, margin: const EdgeInsets.only(right: 10),
                decoration: BoxDecoration(color: sel ? Colors.white : dotColor, shape: BoxShape.circle)),
            Expanded(child: Text(env.name, overflow: TextOverflow.ellipsis,
                style: TextStyle(color: sel ? Colors.white : c.textPrimary, fontSize: 12.5, fontWeight: FontWeight.w500))),
            Text('org ${env.orgId} · $sub',
                style: _mono(sel ? Colors.white70 : c.textSecondary, 10.5)),
          ]),
        ),
      ),
    );
  }
}
```

Delete the now-unused `_SidebarRow`. Update `_Sidebar`'s constructor to accept `final void Function(String folder, String env) onSelectEnv;` and thread it into the `_EnvRow`s.

- [ ] **Step 5: Env-scope the connection panes**

In `_ConnMain` and its children, source the env-specific fields from the selected env. Concretely:
- `_ConnMain.build`: replace `final item = state.selected;` with `final item = state.selected; final env = state.selectedEnvSummary;` and the null guard `if (item == null)` → `if (item == null || env == null)`. Pass `env` to `_ConnBar`, `_OverviewPanel`, `_SyncLogCard`. Change the "No connection selected" copy to "No environment selected". Keep the Files tab `rootFolder: item.summary.folder` (Phase 1 browses the project root; env-scoping the Files tab is Phase 2).
- `_ConnBar`: add `final EnvSummary env;`. Title becomes `'${item.summary.name} · ${env.name}'`; the subtitle host/org line uses `env.apiBase`/`env.orgId`. The Sync button calls `onSync(item, env)` and its disabled/label state uses `_statusOf(state, item, env)`. Change `onSync`'s type to `void Function(ProjectItem, EnvSummary)`; `onEdit`/`onReveal`/`onRemove` take `ProjectItem`.
- `_OverviewPanel`: add `final EnvSummary env;`; read `env.fileCount`, `env.lastSyncUnix`, `env.authKind`; pass `env` to its `_SyncLogCard`.
- `_SyncLogCard`: add `final EnvSummary env;`; change the `syncLog`/`syncMessage` lookups and `_statusOf` to use `widget.state.envKey(widget.item.summary.folder, widget.env.name)` and `_statusOf(widget.state, widget.item, widget.env)`; the "up to date" line uses `widget.env.lastSyncUnix`.
- `_editConnection` in `_HomePageState` opens `EditConnectionDialog(state: state, item: i, env: state.selectedEnvSummary!)` (add the `env` arg the dialog now requires).

- [ ] **Step 6: Env-scope the Fleet overview (keep it compiling)**

`_FleetView`/`_FleetTable`/`_FleetRow` iterate `state.connections`. For Phase 1, flatten to one row per (project, env): in `_FleetView.build`, build `final rows = [for (final p in state.projects) for (final e in p.summary.envs) (p, e)];` and compute `files`/`errors`/`syncedToday` over `rows` (using `env.fileCount`, `env.lastSyncUnix`, and `state.syncState[state.envKey(p.summary.folder, e.name)]`). `_FleetTable`/`_FleetRow` take `(ProjectItem, EnvSummary)` rows; the "Connection" column shows `'${p.summary.name} · ${e.name}'`; tapping calls `onOpenConn(p.summary.folder, e.name)` (change `onOpenConn` to `void Function(String folder, String env)`, wired to `state.selectEnv`). Rename the "Connections" stat card label to "Environments". `_FleetView`'s "New connection" button label → "New project".

- [ ] **Step 7: Run `flutter analyze` to verify the app compiles**

Run: `cd desktop && flutter analyze`
Expected: "No issues found!" Fix any remaining `ConnItem`/`.sync(`/`onSelectConn` references it reports until clean.

- [ ] **Step 8: Run the widget test to verify it passes**

Run: `cd desktop && flutter test test/sidebar_multi_env_test.dart test/app_state_test.dart`
Expected: PASS.

- [ ] **Step 9: Commit**

```bash
git add desktop/lib/src/home_page.dart desktop/test/sidebar_multi_env_test.dart
git commit -m "feat(desktop): two-level project→env sidebar with per-env selection and sync"
```

---

### Task 5: Update goldens + bridge integration test to the new model

**Files:**
- Modify: `desktop/test/golden_mdh_test.dart` (seed projects/envs)
- Regenerate: `desktop/test/goldens/*.png`
- Modify: `desktop/integration_test/bridge_test.dart` (new fn names + a multi-env case)

**Interfaces:**
- Consumes: everything from Tasks 2–4.

- [ ] **Step 1: Update golden seeding**

In `golden_mdh_test.dart`, replace the `_conn(...)` helper and `_seeded()`/`_filesState()` to build `ProjectItem`/`ProjectSummary`/`EnvSummary` and set `selectProject(...)`/`selectEnv(...)`. Replace `_conn` with:

```dart
ProjectItem _proj(String name, List<EnvSummary> envs, {bool external = false, String? folder}) =>
    ProjectItem(ProjectSummary(id: name, name: name, folder: folder ?? '/tmp/Rossum/$name', envs: envs), external);

EnvSummary _env(String name, int org, {int? lastSync, int files = 0}) => EnvSummary(
      name: name, apiBase: 'https://acme.rossum.app/api/v1', orgId: BigInt.from(org),
      authKind: AuthKind.token, lastSyncUnix: lastSync, fileCount: BigInt.from(files));
```

Rebuild `_seeded()` so at least one project is multi-env (to exercise the tree), set `s.projects = [...]`, `s.selectProject(sel)` then `s.selectEnv(sel, 'prod')`, and key the error/log maps with `s.envKey(folder, env)`. Rebuild `_filesState()` with a single-env `main` project and `selectProject(root.path)`. Update the `_filesFixture()` path if needed (it already writes under `envs/main`, which remains valid). Where the golden calls previously passed `MdhScaffold(state: ..., view: ...)`, add `onSelectEnv: (f, e) => {}` where a tap-less render is fine.

- [ ] **Step 2: Regenerate the goldens**

Run: `cd desktop && flutter test --update-goldens test/golden_mdh_test.dart`
Expected: PASS; the PNGs under `test/goldens/` are rewritten to show the project→env sidebar. **Visually inspect** the updated `mdh_conn_light.png` to confirm the two-level sidebar renders (project row + indented env rows, one selected).

- [ ] **Step 3: Update the bridge integration test**

In `integration_test/bridge_test.dart`: rename `addConnection`→`addProject`, `listConnections`→`listProjects`, `editConnection`→`editProject` (add `env: 'main'`), `trashConnection`→`trashProject`. Adjust assertions from `.orgId`/`.apiBase` on the summary to the first env: e.g. `expect(added.envs.first.orgId, BigInt.from(42))`, `expect(added.envs.first.apiBase, 'https://example.test/api/v1')`. Add one multi-env case:

```dart
test('listProjects surfaces a CLI project with no main env', () async {
  final parent = Directory.systemTemp.createTempSync('rdc_it_multienv');
  try {
    final dir = Directory('${parent.path}/cli')..createSync();
    File('${dir.path}/rdc.toml').writeAsStringSync(
      '[envs.dev]\napi_base = "https://d.test/api/v1"\norg_id = 1\n'
      '[envs.prod]\napi_base = "https://p.test/api/v1"\norg_id = 2\n');
    final list = await listProjects(parent: parent.path);
    expect(list.length, 1);
    final envs = list.first.envs.map((e) => e.name).toList();
    expect(envs, ['dev', 'prod']);
  } finally {
    parent.deleteSync(recursive: true);
  }
});
```

- [ ] **Step 4: Run the full desktop test suite**

Run:
```bash
cd desktop && flutter analyze && flutter test
cd desktop/rust && cargo test
```
Expected: analyze clean; `flutter test` (golden + unit + widget) green; `cargo test` green. Run `flutter test integration_test -d macos` if a host with the native lib is available (verifies `bridge_test.dart` against the real bridge).

- [ ] **Step 5: Commit**

```bash
git add desktop/test/golden_mdh_test.dart desktop/test/goldens desktop/integration_test/bridge_test.dart
git commit -m "test(desktop): goldens + bridge test cover projects with multiple envs"
```

---

## Self-review notes (author)

- **Spec coverage (Phase 1 slice):** §5 data model → Tasks 1–3; §6 discovery (all envs, no-`main` visible) → Task 1 + Task 5 Step 3; §5 `(folder,env)` keying → Task 3; §7.1 two-level sidebar + §7.3 env view (pull-only) → Task 4; §11 backward compat (single-env `main` still works) → covered by Task 5's `_filesState` single-env golden + the retained `add_project`→`main`. Deferred to later plans (called out in this plan): §4/§7.2 Project view, §6 drift status, §8 `add_env`/`remove_env`/promote ops, §9–§10 promote + Configure, §4 terminology of internal types beyond user-facing labels.
- **Not in Phase 1 (by design):** `DriftStatus` (spec §5) — env status stays synced/never/running as today; drift lands with the Project view (Phase 2) where it's first shown. The `is_external` flag stays a Dart-side concept (`ProjectItem.isExternal`), matching today's `ConnItem`.
- **Type consistency:** Rust `sync_env(folder, env, api_base, org_id, sink)` ↔ Dart `syncEnv(folder:, env:, apiBase:, orgId:)`; `editProject(folder, env, input)` ↔ `editProject(folder:, env:, input:)`; `ProjectSummary.envs: Vec<EnvSummary>` ↔ Dart `ProjectSummary.envs: List<EnvSummary>`. `AppState.envKey` is the single keying helper used by both sync and every status read.
