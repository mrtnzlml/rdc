# rdc M12 — Mapping + Plan + Apply (Deploy Workflow) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Close the original user request — "push these changes to another Rossum.ai account (deployment from TEST to PROD)". M12 adds three commands: `rdc map`, `rdc plan`, `rdc apply`. The mapping file connects src-slug↔tgt-slug per kind. Plan shows what apply would do. Apply pushes src hooks (with tgt overlay applied) to tgt's API.

**Architecture:** A new `mapping` module owns the TOML schema and load/save. A new `cli/deploy/` module hosts the three commands. Apply reuses M10's push pattern: serialize hook → apply tgt overlay → PATCH via tgt's API client. Per-kind slug↔slug map; the tgt slug + tgt lockfile gives us the tgt id for the API call.

**Tech Stack:** Same as M11.

**Scope:**
- ✅ `rdc map <src> <tgt>` — auto-match by slug, write `.rdc/map/<src>→<tgt>.toml`
- ✅ `rdc plan --from <src> --to <tgt>` — show what apply would do
- ✅ `rdc apply --from <src> --to <tgt>` — execute hook PATCHes against tgt
- ✅ tgt overlay applied during apply
- ❌ NOT drift detection between local tgt snapshot and remote tgt
- ❌ NOT overlay-managed diff exclusion (just always apply tgt overlay)
- ❌ NOT new / deleted handling (only updates of pre-mapped existing hooks)
- ❌ NOT two-phase send
- ❌ NOT idempotent apply (each apply PATCHes mapped hooks unconditionally)
- ❌ NOT kinds beyond hooks (matches push scope)

**End state of M12:**

```
$ rdc map test prod
Auto-matched 2 hooks by slug. Wrote .rdc/map/test→prod.toml.

$ rdc plan --from test --to prod
Plan: TEST → PROD
  ~ hooks/validator-invoices  →  prod/validator-invoices (id 401)
  ~ hooks/sftp-import          →  prod/sftp-import (id 402)

$ rdc apply --from test --to prod
Applied 2 hook PATCHes from test to prod
```

---

## File Structure

| Path | Status | Responsibility |
|---|---|---|
| `src/mapping.rs` | Create | `Mapping` model, TOML load/save, slug→slug per kind |
| `src/lib.rs` | Modify | Re-export `mapping` |
| `src/paths.rs` | Modify | Add `mapping_dir()` and `mapping_file(src, tgt)` |
| `src/cli/deploy/mod.rs` | Create | Top-level dispatcher / shared utilities |
| `src/cli/deploy/map.rs` | Create | `rdc map` impl |
| `src/cli/deploy/plan.rs` | Create | `rdc plan` impl |
| `src/cli/deploy/apply.rs` | Create | `rdc apply` impl |
| `src/cli/mod.rs` | Modify | Add Map / Plan / Apply subcommands |
| `tests/cli_deploy.rs` | Create | Integration tests for the three commands |
| `README.md` | Modify | Document the deploy workflow |

---

## Task 1: `mapping` module + paths

**Files:**
- Create: `src/mapping.rs`
- Modify: `src/lib.rs`
- Modify: `src/paths.rs`

- [ ] **Step 1: Create `src/mapping.rs`**

```rust
//! Env-pair mapping — connects src slug ↔ tgt slug per kind. Written by
//! `rdc map`, consumed by `rdc plan` / `rdc apply`. Per spec §10.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::Path;

#[derive(Debug, Serialize, Deserialize, PartialEq, Clone)]
pub struct Mapping {
    pub version: u32,
    /// Per-kind: src_slug → tgt_slug.
    #[serde(default)]
    pub hooks: BTreeMap<String, String>,
}

impl Default for Mapping {
    fn default() -> Self {
        Self { version: 1, hooks: BTreeMap::new() }
    }
}

impl Mapping {
    pub fn load(path: &Path) -> Result<Self> {
        if !path.exists() {
            return Ok(Self::default());
        }
        let raw = std::fs::read_to_string(path)
            .with_context(|| format!("reading {}", path.display()))?;
        let m: Mapping = toml::from_str(&raw)
            .with_context(|| format!("parsing {}", path.display()))?;
        Ok(m)
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        let s = toml::to_string_pretty(self)
            .context("serializing mapping")?;
        crate::snapshot::writer::write_atomic(path, s.as_bytes())?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn load_returns_default_when_missing() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("nope.toml");
        let m = Mapping::load(&path).unwrap();
        assert_eq!(m, Mapping::default());
    }

    #[test]
    fn round_trip() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("test_to_prod.toml");
        let mut m = Mapping::default();
        m.hooks.insert("validator-invoices".into(), "validator-invoices".into());
        m.hooks.insert("sftp-import".into(), "sftp-import-prod".into());
        m.save(&path).unwrap();
        let loaded = Mapping::load(&path).unwrap();
        assert_eq!(loaded, m);
    }
}
```

- [ ] **Step 2: Re-export from `src/lib.rs`**

```rust
pub mod mapping;
```

(insert alphabetically near `model`).

- [ ] **Step 3: Add path accessors**

In `src/paths.rs`, add inside `impl Paths`:

```rust
    /// `<root>/.rdc/map/`
    pub fn mapping_dir(&self) -> PathBuf {
        self.root.join(".rdc").join("map")
    }

    /// `<root>/.rdc/map/<src>→<tgt>.toml`
    /// Note: uses U+2192 RIGHTWARDS ARROW for unambiguous filenames.
    pub fn mapping_file(&self, src: &str, tgt: &str) -> PathBuf {
        self.mapping_dir().join(format!("{src}→{tgt}.toml"))
    }
```

In `mod tests`:

```rust
    #[test]
    fn mapping_dir_path() {
        assert_eq!(p().mapping_dir(), Path::new("/proj/.rdc/map"));
    }

    #[test]
    fn mapping_file_path() {
        assert_eq!(
            p().mapping_file("test", "prod"),
            Path::new("/proj/.rdc/map/test→prod.toml")
        );
    }
```

- [ ] **Step 4: Run tests**

Run: `. "$HOME/.cargo/env" && cargo test --lib`
Expected: all lib tests pass + new mapping (2) + paths (2) tests.

- [ ] **Step 5: Commit**

```bash
git add src/lib.rs src/mapping.rs src/paths.rs
git commit -m "feat(mapping): per-env-pair slug mapping store"
```

---

## Task 2: `rdc map` command

**Files:**
- Create: `src/cli/deploy/mod.rs`
- Create: `src/cli/deploy/map.rs`
- Modify: `src/cli/mod.rs`

- [ ] **Step 1: Create `cli/deploy/mod.rs`**

```rust
pub mod apply;
pub mod map;
pub mod plan;
```

- [ ] **Step 2: Create `cli/deploy/map.rs`**

```rust
use crate::config::ProjectConfig;
use crate::mapping::Mapping;
use crate::paths::Paths;
use anyhow::{anyhow, Context, Result};
use std::collections::BTreeMap;

/// `rdc map <src> <tgt>` — auto-match hooks by slug. Reads both env
/// snapshot directories (`envs/<src>/hooks/` and `envs/<tgt>/hooks/`); for
/// each src hook with a matching slug in tgt, adds the entry to the mapping
/// file. Existing entries are preserved.
pub async fn run(src: &str, tgt: &str) -> Result<()> {
    let cwd = std::env::current_dir().context("getting current directory")?;
    let src_paths = Paths::for_env(&cwd, src);
    let tgt_paths = Paths::for_env(&cwd, tgt);

    // Validate both envs exist in rdc.toml (sanity check).
    let cfg = ProjectConfig::load(&src_paths.project_config())
        .with_context(|| format!("loading project config from {}", src_paths.project_config().display()))?;
    if !cfg.envs.contains_key(src) {
        return Err(anyhow!("env '{src}' is not defined in rdc.toml"));
    }
    if !cfg.envs.contains_key(tgt) {
        return Err(anyhow!("env '{tgt}' is not defined in rdc.toml"));
    }

    let src_hooks = list_hook_slugs(&src_paths.hooks_dir())?;
    let tgt_hooks = list_hook_slugs(&tgt_paths.hooks_dir())?;
    let tgt_set: std::collections::HashSet<_> = tgt_hooks.iter().cloned().collect();

    let mapping_path = src_paths.mapping_file(src, tgt);
    let mut mapping = Mapping::load(&mapping_path)?;

    let pre_count = mapping.hooks.len();
    let mut newly_matched: BTreeMap<String, String> = BTreeMap::new();
    for src_slug in &src_hooks {
        if mapping.hooks.contains_key(src_slug) {
            continue; // already mapped, leave alone
        }
        if tgt_set.contains(src_slug) {
            newly_matched.insert(src_slug.clone(), src_slug.clone());
        }
    }
    let new_count = newly_matched.len();
    mapping.hooks.extend(newly_matched);

    if !mapping.hooks.is_empty() {
        std::fs::create_dir_all(src_paths.mapping_dir())
            .with_context(|| format!("creating {}", src_paths.mapping_dir().display()))?;
        mapping.save(&mapping_path)?;
    }

    println!(
        "Auto-matched {} new hooks by slug ({} total in mapping). Wrote {}.",
        new_count,
        pre_count + new_count,
        mapping_path.display()
    );
    Ok(())
}

fn list_hook_slugs(hooks_dir: &std::path::Path) -> Result<Vec<String>> {
    if !hooks_dir.exists() {
        return Ok(Vec::new());
    }
    let mut out = Vec::new();
    for entry in std::fs::read_dir(hooks_dir)
        .with_context(|| format!("reading {}", hooks_dir.display()))?
    {
        let entry = entry
            .with_context(|| format!("listing {}", hooks_dir.display()))?;
        let name = entry.file_name().to_string_lossy().to_string();
        if let Some(slug) = name.strip_suffix(".json") {
            if !slug.ends_with(".remote") {
                out.push(slug.to_string());
            }
        }
    }
    out.sort();
    Ok(out)
}
```

- [ ] **Step 3: Add Map subcommand to `cli/mod.rs`**

Replace `src/cli/mod.rs` to add the three new subcommands and the `deploy` module:

```rust
use clap::{Parser, Subcommand};

#[derive(Debug, Parser)]
#[command(name = "rdc", version, about = "Rossum Deployment as Code")]
pub struct Cli {
    #[command(subcommand)]
    pub command: Option<Command>,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Bootstrap a new rdc project in the current directory.
    Init {
        #[arg(long)]
        name: String,
        #[arg(long = "env", value_name = "ENV_SPEC", required = true)]
        envs: Vec<String>,
    },
    /// Pull a Rossum environment's configuration into the local snapshot.
    Pull {
        env: String,
    },
    /// Push locally-edited hooks back to the Rossum environment.
    Push {
        env: String,
    },
    /// Auto-match hooks by slug between two envs and write the mapping file.
    Map {
        src: String,
        tgt: String,
    },
    /// Show what `rdc apply --from <src> --to <tgt>` would do.
    Plan {
        #[arg(long)]
        from: String,
        #[arg(long)]
        to: String,
    },
    /// Push src env's hooks (with tgt overlay applied) to tgt env per the mapping.
    Apply {
        #[arg(long)]
        from: String,
        #[arg(long)]
        to: String,
    },
}

pub async fn run(cli: Cli) -> anyhow::Result<()> {
    match cli.command {
        Some(Command::Init { name, envs }) => crate::cli::init::run(&name, &envs).await,
        Some(Command::Pull { env }) => crate::cli::pull::run(&env).await,
        Some(Command::Push { env }) => crate::cli::push::run(&env).await,
        Some(Command::Map { src, tgt }) => crate::cli::deploy::map::run(&src, &tgt).await,
        Some(Command::Plan { from, to }) => crate::cli::deploy::plan::run(&from, &to).await,
        Some(Command::Apply { from, to }) => crate::cli::deploy::apply::run(&from, &to).await,
        None => {
            use clap::CommandFactory;
            Cli::command().print_help()?;
            println!();
            Ok(())
        }
    }
}

pub mod deploy;
pub mod index;
pub mod init;
pub mod pull;
pub mod push;
```

- [ ] **Step 4: Run tests**

Run: `. "$HOME/.cargo/env" && cargo build`

`rdc plan` and `rdc apply` modules don't exist yet — Tasks 3 + 4 add them. Build will fail. Add stubs first:

Create `src/cli/deploy/plan.rs`:

```rust
use anyhow::Result;

pub async fn run(_from: &str, _to: &str) -> Result<()> {
    anyhow::bail!("plan not yet implemented (M12 task 3)")
}
```

Create `src/cli/deploy/apply.rs`:

```rust
use anyhow::Result;

pub async fn run(_from: &str, _to: &str) -> Result<()> {
    anyhow::bail!("apply not yet implemented (M12 task 4)")
}
```

Now run: `. "$HOME/.cargo/env" && cargo test`
Expected: all existing tests pass.

- [ ] **Step 5: Commit**

```bash
git add src/cli/
git commit -m "feat(cli): rdc map (auto-match hooks by slug); plan+apply stubs"
```

---

## Task 3: `rdc plan` command

**Files:**
- Modify: `src/cli/deploy/plan.rs`

- [ ] **Step 1: Implement `plan`**

Replace `src/cli/deploy/plan.rs`:

```rust
use crate::config::ProjectConfig;
use crate::mapping::Mapping;
use crate::overlay::Overlay;
use crate::paths::Paths;
use crate::snapshot::hook::read_hook;
use crate::state::Lockfile;
use anyhow::{anyhow, Context, Result};

/// Show what `rdc apply --from <src> --to <tgt>` would do.
/// Read-only: no API calls, no disk writes.
pub async fn run(src: &str, tgt: &str) -> Result<()> {
    let cwd = std::env::current_dir().context("getting current directory")?;
    let src_paths = Paths::for_env(&cwd, src);
    let tgt_paths = Paths::for_env(&cwd, tgt);

    let cfg = ProjectConfig::load(&src_paths.project_config())
        .with_context(|| format!("loading project config from {}", src_paths.project_config().display()))?;
    if !cfg.envs.contains_key(src) {
        return Err(anyhow!("env '{src}' is not defined in rdc.toml"));
    }
    if !cfg.envs.contains_key(tgt) {
        return Err(anyhow!("env '{tgt}' is not defined in rdc.toml"));
    }

    let mapping = Mapping::load(&src_paths.mapping_file(src, tgt))?;
    let tgt_lockfile = Lockfile::load(&tgt_paths.lockfile())?;

    println!("Plan: {} → {}", src, tgt);

    let mut count = 0;
    let mut warnings = 0;
    for (src_slug, tgt_slug) in &mapping.hooks {
        // Verify src snapshot has the hook.
        let src_hook_path = src_paths.hooks_dir().join(format!("{src_slug}.json"));
        if !src_hook_path.exists() {
            eprintln!("warning: src hooks/{src_slug}.json missing — skipping in plan");
            warnings += 1;
            continue;
        }

        // Verify tgt lockfile has the slug (so we know the id).
        let tgt_id = tgt_lockfile
            .objects
            .get("hooks")
            .and_then(|m| m.get(tgt_slug))
            .map(|e| e.id);
        let Some(tgt_id) = tgt_id else {
            eprintln!("warning: tgt lockfile has no entry for hooks/{tgt_slug} — run `rdc pull {tgt}` first");
            warnings += 1;
            continue;
        };

        println!("  ~ hooks/{src_slug}  →  {tgt}/{tgt_slug} (id {tgt_id})");
        count += 1;
    }

    let _ = read_hook;          // suppress unused-import warning
    let _ = Overlay::load;      // suppress unused-import warning
    if count == 0 && warnings == 0 {
        println!("  (no mapped hooks)");
    }
    Ok(())
}
```

(The unused-import suppression at the bottom is silly; instead, just drop the unused imports. Cleaner version of the imports:)

```rust
use crate::config::ProjectConfig;
use crate::mapping::Mapping;
use crate::paths::Paths;
use crate::state::Lockfile;
use anyhow::{anyhow, Context, Result};
```

(Remove the `read_hook` and `Overlay` imports — not used in plan, only in apply.)

Final `plan.rs`:

```rust
use crate::config::ProjectConfig;
use crate::mapping::Mapping;
use crate::paths::Paths;
use crate::state::Lockfile;
use anyhow::{anyhow, Context, Result};

pub async fn run(src: &str, tgt: &str) -> Result<()> {
    let cwd = std::env::current_dir().context("getting current directory")?;
    let src_paths = Paths::for_env(&cwd, src);
    let tgt_paths = Paths::for_env(&cwd, tgt);

    let cfg = ProjectConfig::load(&src_paths.project_config())
        .with_context(|| format!("loading project config from {}", src_paths.project_config().display()))?;
    if !cfg.envs.contains_key(src) {
        return Err(anyhow!("env '{src}' is not defined in rdc.toml"));
    }
    if !cfg.envs.contains_key(tgt) {
        return Err(anyhow!("env '{tgt}' is not defined in rdc.toml"));
    }

    let mapping = Mapping::load(&src_paths.mapping_file(src, tgt))?;
    let tgt_lockfile = Lockfile::load(&tgt_paths.lockfile())?;

    println!("Plan: {src} → {tgt}");

    let mut count = 0;
    let mut warnings = 0;
    for (src_slug, tgt_slug) in &mapping.hooks {
        let src_hook_path = src_paths.hooks_dir().join(format!("{src_slug}.json"));
        if !src_hook_path.exists() {
            eprintln!("warning: src hooks/{src_slug}.json missing — skipping in plan");
            warnings += 1;
            continue;
        }
        let tgt_id = tgt_lockfile
            .objects
            .get("hooks")
            .and_then(|m| m.get(tgt_slug))
            .map(|e| e.id);
        let Some(tgt_id) = tgt_id else {
            eprintln!("warning: tgt lockfile has no entry for hooks/{tgt_slug} — run `rdc pull {tgt}` first");
            warnings += 1;
            continue;
        };
        println!("  ~ hooks/{src_slug}  →  {tgt}/{tgt_slug} (id {tgt_id})");
        count += 1;
    }

    if count == 0 && warnings == 0 {
        println!("  (no mapped hooks)");
    }
    Ok(())
}
```

- [ ] **Step 2: Run tests**

Run: `. "$HOME/.cargo/env" && cargo test`
Expected: all tests pass.

- [ ] **Step 3: Commit**

```bash
git add src/cli/deploy/plan.rs
git commit -m "feat(cli): rdc plan (read-only deploy preview)"
```

---

## Task 4: `rdc apply` command

**Files:**
- Modify: `src/cli/deploy/apply.rs`

- [ ] **Step 1: Implement `apply`**

Replace `src/cli/deploy/apply.rs`:

```rust
use crate::api::RossumClient;
use crate::config::ProjectConfig;
use crate::mapping::Mapping;
use crate::overlay::{apply_overrides, Overlay};
use crate::paths::Paths;
use crate::secrets::resolve_token;
use crate::snapshot::hook::read_hook;
use crate::state::Lockfile;
use anyhow::{anyhow, Context, Result};

pub async fn run(src: &str, tgt: &str) -> Result<()> {
    let cwd = std::env::current_dir().context("getting current directory")?;
    let src_paths = Paths::for_env(&cwd, src);
    let tgt_paths = Paths::for_env(&cwd, tgt);

    let cfg = ProjectConfig::load(&src_paths.project_config())
        .with_context(|| format!("loading project config from {}", src_paths.project_config().display()))?;
    let _src_cfg = cfg.envs.get(src).ok_or_else(|| anyhow!("env '{src}' is not defined in rdc.toml"))?;
    let tgt_cfg = cfg.envs.get(tgt).ok_or_else(|| anyhow!("env '{tgt}' is not defined in rdc.toml"))?;

    let token = resolve_token(&cwd, tgt)?;
    let tgt_client = RossumClient::new(tgt_cfg.api_base.clone(), token)
        .context("constructing tgt API client")?;

    let mapping = Mapping::load(&src_paths.mapping_file(src, tgt))?;
    let tgt_lockfile = Lockfile::load(&tgt_paths.lockfile())?;
    let tgt_overlay = Overlay::load(&tgt_paths.overlay_file())
        .with_context(|| format!("loading tgt overlay from {}", tgt_paths.overlay_file().display()))?;

    let mut applied = 0;
    let mut skipped = 0;

    for (src_slug, tgt_slug) in &mapping.hooks {
        let tgt_id = tgt_lockfile
            .objects
            .get("hooks")
            .and_then(|m| m.get(tgt_slug))
            .map(|e| e.id);
        let Some(tgt_id) = tgt_id else {
            eprintln!("warning: tgt lockfile has no entry for hooks/{tgt_slug} — skipping");
            skipped += 1;
            continue;
        };

        let src_hook = match read_hook(&src_paths.hooks_dir(), src_slug) {
            Ok(h) => h,
            Err(e) => {
                eprintln!("warning: cannot read src hooks/{src_slug}: {e:#}");
                skipped += 1;
                continue;
            }
        };

        // Apply tgt overlay (if any).
        let mut payload = serde_json::to_value(&src_hook)
            .context("serializing src hook to value")?;
        if let Some(ov) = &tgt_overlay {
            if let Some(hook_overrides) = ov.hook(tgt_slug) {
                apply_overrides(&mut payload, hook_overrides);
            }
        }
        let payload_hook: crate::model::Hook = serde_json::from_value(payload)
            .with_context(|| format!("re-deserializing overlay-applied hook for tgt slug '{tgt_slug}'"))?;

        // PATCH tgt's hook by id.
        tgt_client.update_hook(tgt_id, &payload_hook).await
            .with_context(|| format!("PATCH tgt hooks/{tgt_id} (mapped from src '{src_slug}')"))?;
        applied += 1;
    }

    let mut summary = format!("Applied {applied} hook PATCHes from {src} to {tgt}");
    if skipped > 0 {
        summary.push_str(&format!(", {skipped} skipped"));
    }
    println!("{summary}");
    Ok(())
}
```

- [ ] **Step 2: Run tests**

Run: `. "$HOME/.cargo/env" && cargo test`
Expected: all tests pass.

- [ ] **Step 3: Commit**

```bash
git add src/cli/deploy/apply.rs
git commit -m "feat(cli): rdc apply (push src hooks to tgt with overlay applied)"
```

---

## Task 5: Integration tests

**Files:**
- Create: `tests/cli_deploy.rs`

- [ ] **Step 1: Create integration tests**

```rust
use assert_cmd::Command;
use predicates::prelude::*;
use tempfile::TempDir;
use wiremock::matchers::{header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn fixture(name: &str) -> serde_json::Value {
    let raw = std::fs::read_to_string(format!("testdata/fixtures/{name}")).unwrap();
    serde_json::from_str(&raw).unwrap()
}

fn empty_list() -> serde_json::Value {
    serde_json::json!({ "pagination": { "next": null }, "results": [] })
}

async fn mount_full_pull(server: &MockServer, hooks_payload: serde_json::Value) {
    Mock::given(method("GET"))
        .and(path("/api/v1/organizations/1"))
        .respond_with(ResponseTemplate::new(200).set_body_json(fixture("organization.json")))
        .mount(server).await;
    Mock::given(method("GET"))
        .and(path("/api/v1/hooks"))
        .respond_with(ResponseTemplate::new(200).set_body_json(hooks_payload))
        .mount(server).await;
    for ep in [
        "/api/v1/workspaces", "/api/v1/queues",
        "/api/v1/rules", "/api/v1/labels", "/api/v1/engines", "/api/v1/engine_fields",
        "/api/v1/workflows", "/api/v1/workflow_steps", "/api/v1/email_templates",
    ] {
        Mock::given(method("GET"))
            .and(path(ep))
            .respond_with(ResponseTemplate::new(200).set_body_json(empty_list()))
            .mount(server).await;
    }
}

/// Set up a project with two envs (test and prod), pull both, then run the
/// three deploy commands.
#[tokio::test]
async fn map_plan_apply_full_flow() {
    use std::sync::{Arc, Mutex};

    let test_server = MockServer::start().await;
    let prod_server = MockServer::start().await;

    mount_full_pull(&test_server, fixture("hooks_list.json")).await;

    // PROD has the same hooks (different IDs) so slug auto-match works.
    let prod_hooks = serde_json::json!({
        "pagination": { "total": 2, "next": null, "previous": null },
        "results": [
            {
                "id": 401,
                "url": "https://prod.rossum.app/api/v1/hooks/401",
                "name": "Validator: invoices",
                "type": "function",
                "queues": [],
                "events": ["annotation_content"],
                "config": { "runtime": "python3.12", "code": "def x(payload):\n    return {}\n" }
            },
            {
                "id": 402,
                "url": "https://prod.rossum.app/api/v1/hooks/402",
                "name": "SFTP import",
                "type": "function",
                "queues": [],
                "events": ["annotation_status"],
                "config": { "runtime": "python3.12", "code": "def import_files():\n    pass\n" }
            }
        ]
    });
    mount_full_pull(&prod_server, prod_hooks).await;

    // PROD must accept PATCH /hooks/401. Capture body so we can assert overlay applied.
    let captured: Arc<Mutex<Option<serde_json::Value>>> = Arc::new(Mutex::new(None));
    let captured_clone = captured.clone();
    Mock::given(method("PATCH"))
        .and(path("/api/v1/hooks/401"))
        .respond_with(move |req: &wiremock::Request| {
            let body: serde_json::Value = serde_json::from_slice(&req.body).unwrap();
            *captured_clone.lock().unwrap() = Some(body.clone());
            ResponseTemplate::new(200).set_body_json(body)
        })
        .mount(&prod_server).await;
    Mock::given(method("PATCH"))
        .and(path("/api/v1/hooks/402"))
        .respond_with(move |req: &wiremock::Request| {
            let body: serde_json::Value = serde_json::from_slice(&req.body).unwrap();
            ResponseTemplate::new(200).set_body_json(body)
        })
        .mount(&prod_server).await;

    let project = TempDir::new().unwrap();
    Command::cargo_bin("rdc").unwrap()
        .current_dir(project.path())
        .args([
            "init",
            "--name", "x",
            "--env", &format!("test={}/api/v1:1", test_server.uri()),
            "--env", &format!("prod={}/api/v1:1", prod_server.uri()),
        ])
        .assert().success();
    std::fs::write(
        project.path().join("secrets/test.secrets.json"),
        r#"{"api_token":"TEST_TOKEN"}"#,
    ).unwrap();
    std::fs::write(
        project.path().join("secrets/prod.secrets.json"),
        r#"{"api_token":"PROD_TOKEN"}"#,
    ).unwrap();

    Command::cargo_bin("rdc").unwrap().current_dir(project.path()).args(["pull", "test"]).assert().success();
    Command::cargo_bin("rdc").unwrap().current_dir(project.path()).args(["pull", "prod"]).assert().success();

    // rdc map test prod — expect 2 auto-matches
    Command::cargo_bin("rdc").unwrap()
        .current_dir(project.path())
        .args(["map", "test", "prod"])
        .assert().success()
        .stdout(predicate::str::contains("Auto-matched 2"));

    let mapping_file = project.path().join(".rdc/map/test→prod.toml");
    assert!(mapping_file.exists());

    // rdc plan
    Command::cargo_bin("rdc").unwrap()
        .current_dir(project.path())
        .args(["plan", "--from", "test", "--to", "prod"])
        .assert().success()
        .stdout(predicate::str::contains("Plan: test → prod"))
        .stdout(predicate::str::contains("validator-invoices"))
        .stdout(predicate::str::contains("(id 401)"));

    // Add a tgt overlay so apply demonstrates the override.
    std::fs::write(
        project.path().join("envs/prod/overlay.toml"),
        r#"
version = 1

[hooks.validator-invoices]
"name" = "Validator (PROD)"
"#,
    ).unwrap();

    // rdc apply
    Command::cargo_bin("rdc").unwrap()
        .current_dir(project.path())
        .args(["apply", "--from", "test", "--to", "prod"])
        .assert().success()
        .stdout(predicate::str::contains("Applied 2"));

    // Captured PATCH body for hook 401 should have the overlay-applied name.
    let body = captured.lock().unwrap().clone().expect("PATCH body for hook 401");
    assert_eq!(body["name"], serde_json::Value::String("Validator (PROD)".into()));
}
```

- [ ] **Step 2: Run all tests**

Run: `. "$HOME/.cargo/env" && cargo test`
Expected: all tests pass — adds 1 new deploy test.

- [ ] **Step 3: Commit**

```bash
git add tests/cli_deploy.rs
git commit -m "test(cli): integration test for rdc map/plan/apply full flow"
```

---

## Task 6: README update

**Files:**
- Modify: `README.md`

- [ ] **Step 1: Update Status + add Deploy section**

Update Status:
```
**Status:** M12. Pull side complete. Push for hooks. Deploy workflow (`rdc map`/`plan`/`apply`) for hooks across envs.
```

Add new section after "Overlays":

```
## Deploy (M12 — TEST → PROD for hooks)

`rdc map <src> <tgt>` — auto-match hook slugs between two envs and write
`.rdc/map/<src>→<tgt>.toml`. The mapping file is hand-editable; entries
that auto-match by slug are added on each run.

`rdc plan --from <src> --to <tgt>` — show what apply would do
(read-only, no API calls).

`rdc apply --from <src> --to <tgt>` — for each mapped hook, read the src
snapshot, apply tgt's overlay, PATCH tgt's API. Used after pushing changes
through TEST and ready to roll them to PROD.

**Typical flow:**

```sh
rdc pull test                          # pull both envs once
rdc pull prod
rdc map test prod                      # auto-match by slug
$EDITOR .rdc/map/test→prod.toml        # hand-curate any rename mappings
rdc plan --from test --to prod         # preview
rdc apply --from test --to prod        # execute
```

**M12 limitations:**
- Hooks only.
- Updates only (no creates / deletes).
- No drift detection between local tgt snapshot and remote tgt.
- No overlay-managed diff exclusion (overlay always overrides).
- Apply is not idempotent — every run PATCHes mapped hooks.
```

- [ ] **Step 2: Commit**

```bash
git add README.md
git commit -m "docs: M12 — deploy workflow (map/plan/apply)"
```

---

## Self-Review

**Spec coverage:**
- §6 CLI surface — `rdc map`, `rdc plan`, `rdc apply` added (hooks only)
- §7.4 plan/apply — partial: no drift detection, no overlay-managed exclusion, no new/deleted, no two-phase, no idempotent apply
- §7.5 mapping wizard — partial: auto-match by slug only, no interactive picker

**Placeholder scan:** No "TBD"/"TODO" patterns.

**Type consistency:** `Mapping { version, hooks: BTreeMap<String, String> }` consistent across Tasks 1-4.

**Scope check:** 6 tasks. Mapping is small (~50 LOC); apply is the meat (~80 LOC); plan is a thin read-only variant; one integration test exercises the full flow.

---

## Next milestones

- **M13:** Pull-side overlay stripping; push extension to remaining kinds; auxiliary commands; cross-ref indexer.
- **M14:** Distribution.
