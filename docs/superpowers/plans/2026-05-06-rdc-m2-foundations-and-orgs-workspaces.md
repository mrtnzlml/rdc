# rdc M2 — Foundations + Organization + Workspaces Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Land the structural refactors recommended by the M1 reviewer (paths helper, fallible client constructor, codec round-trip test, pull-driver extraction, lockfile schema v2 with `url` + content hash) AND extend the snapshot to two new Rossum object types — `Organization` (one per env) and `Workspace` (list per env). Verifies the foundation pattern that subsequent milestones will reuse.

**Architecture:** No new traits in M2 — premature abstraction. Each object kind has its own model file (`src/model/<kind>.rs`), its own snapshot codec module (`src/snapshot/<kind>.rs`), its own API method on `RossumClient`, and its own `pull_<kind>` function in `src/cli/pull/`. Shared concerns are extracted as plain helpers: `paths::Paths` for filesystem layout, `record_object` for lockfile updates. If duplication becomes painful in M3+, refactor then.

**Tech Stack:** Rust 2021. Same dependencies as M1 — `clap`, `reqwest`, `serde`, `tokio`, `wiremock`, `assert_cmd`. Adds `sha2` (0.10) for content hashing in the lockfile.

**What this milestone deliberately omits** (deferred to later milestones):
- Queue / schema / inbox / formula extraction (M3 — the workspace-tree milestone).
- Rules, labels, engines, engine fields (M4).
- Workflows, workflow steps, email templates (M5).
- MDH dataset metadata + indexes (M6).
- Three-way merge for subsequent pulls (still always-overwrite in M2; merge lands in M7).
- Conflict resolver TUI, indexer, push, plan/apply, overlays, mapping (M7-M11).

**End state of M2, demonstrable manually:**

```
$ rdc pull dev
Pulled organization, 3 workspaces, 2 hooks from env 'dev'
$ tree envs/dev -L 2
envs/dev/
├── hooks/
├── organization.json
└── workspaces/
    ├── invoices-ap/
    ├── invoices-credit-notes/
    └── purchase-orders/
$ ls envs/dev/workspaces/invoices-ap/
workspace.json
```

(The empty workspace directories are expected: queues land in M3.)

---

## File Structure

| Path | Status | Responsibility |
|---|---|---|
| `Cargo.toml` | Modify | Add `sha2` dep |
| `src/paths.rs` | Create | Centralized filesystem path helpers |
| `src/lib.rs` | Modify | Re-export new modules |
| `src/api/mod.rs` | Modify | Fallible `new`; add `get_organization`, `list_workspaces` |
| `src/model/organization.rs` | Create | `Organization` struct |
| `src/model/workspace.rs` | Create | `Workspace` struct |
| `src/model/mod.rs` | Modify | Re-export new types |
| `src/model/hook.rs` | Modify | Add `Hook::id`, `Hook::modified_at` accessors |
| `src/snapshot/organization.rs` | Create | Org codec (write + read) |
| `src/snapshot/workspace.rs` | Create | Workspace codec (write + read) |
| `src/snapshot/hook.rs` | Modify | Add `read_hook`; ensure round-trip |
| `src/snapshot/mod.rs` | Modify | Declare new submodules |
| `src/state/lockfile.rs` | Modify | `ObjectEntry` gains `url` + `content_hash`; schema bumped to v2 with v1→v2 migration |
| `src/cli/pull.rs` | Replace | Becomes thin orchestrator delegating to per-kind drivers |
| `src/cli/pull/mod.rs` | Create | Move CLI entry function here; declare submodules |
| `src/cli/pull/hooks.rs` | Create | Per-kind driver: list hooks → write each → update lockfile |
| `src/cli/pull/organization.rs` | Create | Per-kind driver for organization |
| `src/cli/pull/workspaces.rs` | Create | Per-kind driver for workspaces |
| `src/cli/pull/common.rs` | Create | Shared `record_object` and `PullCtx` helpers |
| `tests/cli_pull.rs` | Modify | Extend the pull integration test for org + workspaces |
| `testdata/fixtures/organization.json` | Create | Sample organization detail response |
| `testdata/fixtures/workspaces_list.json` | Create | Sample paginated workspaces response |

A note on the `cli::pull` reorganization: the current single-file `src/cli/pull.rs` becomes a directory module. Rust supports this idiom — a file `src/cli/pull.rs` and a directory `src/cli/pull/` with `mod.rs` are alternative spellings of the same module. We delete the file and create the directory.

---

## Task 1: Add `sha2` dependency

**Files:**
- Modify: `Cargo.toml`

- [ ] **Step 1: Add `sha2` to `[dependencies]`**

Open `Cargo.toml` and add the line `sha2 = "0.10"` under `[dependencies]` (alphabetical order — between `serde_json` and `thiserror`). Final dependency block should look like:

```toml
[dependencies]
anyhow = "1"
clap = { version = "4", features = ["derive"] }
reqwest = { version = "0.12", features = ["json", "rustls-tls"], default-features = false }
serde = { version = "1", features = ["derive"] }
serde_json = "1"
sha2 = "0.10"
thiserror = "1"
tokio = { version = "1", features = ["macros", "rt-multi-thread", "fs"] }
toml = "0.8"
```

- [ ] **Step 2: Verify build**

Run: `. "$HOME/.cargo/env" && cargo build`
Expected: clean build; `sha2` and dependencies (e.g., `digest`, `cpufeatures`) compile and link.

- [ ] **Step 3: Commit**

```bash
git add Cargo.toml Cargo.lock
git commit -m "chore: add sha2 dependency for content hashing"
```

---

## Task 2: `paths::Paths` helper

**Files:**
- Create: `src/paths.rs`
- Modify: `src/lib.rs`

- [ ] **Step 1: Write the failing test alongside the implementation**

Create `src/paths.rs`:

```rust
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
}
```

- [ ] **Step 2: Expose**

Modify `src/lib.rs`. Replace its current content with:

```rust
pub mod api;
pub mod cli;
pub mod config;
pub mod model;
pub mod paths;
pub mod secrets;
pub mod slug;
pub mod snapshot;
pub mod state;
```

(The new line is `pub mod paths;`, alphabetically before `secrets`.)

- [ ] **Step 3: Run tests**

Run: `. "$HOME/.cargo/env" && cargo test paths::tests`
Expected: 8 tests pass.

- [ ] **Step 4: Commit**

```bash
git add Cargo.toml src/lib.rs src/paths.rs
git commit -m "feat(paths): centralize project filesystem layout helpers"
```

---

## Task 3: Migrate `init` and `pull` to use `Paths`

**Files:**
- Modify: `src/cli/init.rs`
- Modify: `src/cli/pull.rs`

- [ ] **Step 1: Migrate `cli::init::run` to use `Paths`**

Replace `src/cli/init.rs`:

```rust
use crate::config::{EnvConfig, ProjectConfig, ProjectMeta};
use crate::paths::Paths;
use crate::snapshot::writer::write_atomic;
use anyhow::{anyhow, Context, Result};
use std::collections::BTreeMap;
use std::path::Path;

pub async fn run(name: &str, env_specs: &[String]) -> Result<()> {
    let cwd = std::env::current_dir().context("getting current directory")?;
    let cfg_path = cwd.join("rdc.toml");
    if cfg_path.exists() {
        return Err(anyhow!(
            "directory is already initialized as an rdc project (rdc.toml exists at {})",
            cfg_path.display()
        ));
    }

    let mut envs = BTreeMap::new();
    for spec in env_specs {
        let (env_name, env_cfg) = parse_env_spec(spec)?;
        envs.insert(env_name, env_cfg);
    }

    let cfg = ProjectConfig {
        project: ProjectMeta { name: name.to_string() },
        envs: envs.clone(),
    };
    cfg.save(&cfg_path)?;

    write_gitignore(&cwd)?;
    std::fs::create_dir_all(cwd.join("secrets"))
        .with_context(|| format!("creating {}", cwd.join("secrets").display()))?;
    for env in envs.keys() {
        let paths = Paths::for_env(&cwd, env);
        std::fs::create_dir_all(paths.env_root())
            .with_context(|| format!("creating {}", paths.env_root().display()))?;
        std::fs::create_dir_all(paths.hooks_dir())
            .with_context(|| format!("creating {}", paths.hooks_dir().display()))?;
    }

    println!(
        "Initialized rdc project '{name}' with envs: {}",
        envs.keys().cloned().collect::<Vec<_>>().join(", ")
    );
    Ok(())
}

fn parse_env_spec(spec: &str) -> Result<(String, EnvConfig)> {
    let (env_name, rest) = spec
        .split_once('=')
        .ok_or_else(|| anyhow!("invalid --env spec '{spec}': expected `<env>=<api_base>:<org_id>`"))?;
    let last_colon = rest
        .rfind(':')
        .ok_or_else(|| anyhow!("invalid --env spec '{spec}': missing :<org_id>"))?;
    let api_base = &rest[..last_colon];
    let org_id_str = &rest[last_colon + 1..];
    let org_id: u64 = org_id_str
        .parse()
        .with_context(|| format!("parsing org_id '{org_id_str}' in spec '{spec}'"))?;
    Ok((
        env_name.to_string(),
        EnvConfig {
            api_base: api_base.to_string(),
            org_id,
            workspace_filter: None,
        },
    ))
}

fn write_gitignore(root: &Path) -> Result<()> {
    let path = root.join(".gitignore");
    let body = "/target\n/secrets\n/.rdc/cache\n";
    if path.exists() {
        let existing = std::fs::read_to_string(&path)
            .with_context(|| format!("reading {}", path.display()))?;
        if existing.contains("/secrets") && existing.contains("/.rdc/cache") {
            return Ok(());
        }
        let mut combined = existing;
        if !combined.ends_with('\n') {
            combined.push('\n');
        }
        combined.push_str(body);
        write_atomic(&path, combined.as_bytes())?;
    } else {
        write_atomic(&path, body.as_bytes())?;
    }
    Ok(())
}
```

The change: hooks-dir creation now goes through `Paths::for_env(...).hooks_dir()` instead of inline `cwd.join("envs").join(env).join("hooks")`. Same logic, single source of truth for the layout.

- [ ] **Step 2: Migrate `cli::pull::run` to use `Paths`**

Replace `src/cli/pull.rs`:

```rust
use crate::api::RossumClient;
use crate::config::ProjectConfig;
use crate::paths::Paths;
use crate::secrets::resolve_token;
use crate::slug::slugify_unique;
use crate::snapshot::hook::write_hook;
use crate::state::{Lockfile, ObjectEntry};
use anyhow::{anyhow, Context, Result};
use std::collections::HashSet;

pub async fn run(env: &str) -> Result<()> {
    let cwd = std::env::current_dir().context("getting current directory")?;
    let paths = Paths::for_env(&cwd, env);

    let cfg = ProjectConfig::load(&paths.project_config())
        .with_context(|| format!("loading project config from {}", paths.project_config().display()))?;

    let env_cfg = cfg
        .envs
        .get(env)
        .ok_or_else(|| anyhow!("env '{env}' is not defined in rdc.toml"))?;

    let token = resolve_token(&cwd, env)?;
    let client = RossumClient::new(env_cfg.api_base.clone(), token);

    let hooks = client
        .list_hooks()
        .await
        .with_context(|| format!("listing hooks for env '{env}'"))?;

    std::fs::create_dir_all(paths.hooks_dir())
        .with_context(|| format!("creating {}", paths.hooks_dir().display()))?;

    let mut lockfile = Lockfile::load(&paths.lockfile())?;

    let mut used_slugs: HashSet<String> = HashSet::new();
    for hook in &hooks {
        let slug = slugify_unique(&hook.name, &used_slugs);
        used_slugs.insert(slug.clone());

        write_hook(&paths.hooks_dir(), &slug, hook)
            .with_context(|| format!("writing hook '{}' to disk", hook.name))?;

        lockfile.upsert(
            "hooks",
            &slug,
            ObjectEntry {
                id: hook.id,
                modified_at: hook
                    .extra
                    .get("modified_at")
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string()),
            },
        );
    }

    lockfile.save(&paths.lockfile())?;

    println!("Pulled {} hooks from env '{env}'", hooks.len());
    Ok(())
}
```

- [ ] **Step 3: Run the full suite — nothing should regress**

Run: `. "$HOME/.cargo/env" && cargo test`
Expected: all 35 tests still pass (same suite as M1).

- [ ] **Step 4: Commit**

```bash
git add src/cli/
git commit -m "refactor(cli): route file paths through paths::Paths helper"
```

---

## Task 4: Make `RossumClient::new` fallible

**Files:**
- Modify: `src/api/mod.rs`
- Modify: `src/cli/pull.rs`

- [ ] **Step 1: Change `RossumClient::new` signature**

Open `src/api/mod.rs` and locate the `impl RossumClient { pub fn new(...) -> Self }` block. Replace the `new` method with:

```rust
    pub fn new(base_url: String, token: String) -> Result<Self> {
        let http = Client::builder()
            .build()
            .map_err(|e| anyhow::anyhow!("building reqwest client: {e}"))?;
        Ok(Self { base_url, token, http })
    }
```

The change: returns `Result<Self>` and surfaces the (currently never-occurring) builder error properly instead of `expect()`.

- [ ] **Step 2: Update `cli::pull::run` to handle the Result**

In `src/cli/pull.rs`, the line currently reads:

```rust
let client = RossumClient::new(env_cfg.api_base.clone(), token);
```

Replace it with:

```rust
let client = RossumClient::new(env_cfg.api_base.clone(), token)
    .context("constructing Rossum API client")?;
```

- [ ] **Step 3: Update `tests/api_hooks.rs` to handle the Result**

Find each `RossumClient::new(...)` call in `tests/api_hooks.rs` (there are two). Add `.unwrap()`:

```rust
let client = RossumClient::new(format!("{}/api/v1", server.uri()), "TEST_TOKEN".into()).unwrap();
```

```rust
let client = RossumClient::new(format!("{}/api/v1", server.uri()), "BAD".into()).unwrap();
```

- [ ] **Step 4: Run the full suite**

Run: `. "$HOME/.cargo/env" && cargo test`
Expected: all 35 tests still pass.

- [ ] **Step 5: Commit**

```bash
git add src/ tests/
git commit -m "refactor(api): make RossumClient::new fallible"
```

---

## Task 5: Hook codec round-trip — add `read_hook` and a property test

**Files:**
- Modify: `src/snapshot/hook.rs`

This task adds the read half of the hook codec and a round-trip test, validating the pattern before we replicate it for organization and workspace.

- [ ] **Step 1: Add `read_hook` and round-trip tests**

Open `src/snapshot/hook.rs`. Append the following at the END of the existing file (after the `write_hook` function definition and before the existing `#[cfg(test)] mod tests` block):

```rust
/// Read a hook back from disk: load `<dir>/<slug>.json`, then if `<dir>/<slug>.py`
/// exists, splice its contents back into `config.code` so the in-memory `Hook`
/// is byte-for-byte equivalent to what was originally serialized.
pub fn read_hook(dir: &Path, slug: &str) -> Result<Hook> {
    let json_path = dir.join(format!("{slug}.json"));
    let raw = std::fs::read_to_string(&json_path)
        .with_context(|| format!("reading {}", json_path.display()))?;
    let mut value: Value = serde_json::from_str(&raw)
        .with_context(|| format!("parsing {}", json_path.display()))?;

    let py_path = dir.join(format!("{slug}.py"));
    if py_path.exists() {
        let code = std::fs::read_to_string(&py_path)
            .with_context(|| format!("reading {}", py_path.display()))?;
        // Strip the trailing newline we added on write so round-trip is exact.
        let code = code.strip_suffix('\n').unwrap_or(&code).to_string();
        if let Some(config) = value.get_mut("config").and_then(|c| c.as_object_mut()) {
            config.insert("code".to_string(), Value::String(code));
        }
    }

    let hook: Hook = serde_json::from_value(value)
        .with_context(|| format!("deserializing hook from {}", json_path.display()))?;
    Ok(hook)
}
```

Then, INSIDE the existing `#[cfg(test)] mod tests` block, add four new tests at the end of the block (just before the closing `}` of `mod tests`):

```rust
    #[test]
    fn round_trip_with_code() {
        let dir = TempDir::new().unwrap();
        let original = sample_hook();
        write_hook(dir.path(), "sample", &original).unwrap();
        let read = read_hook(dir.path(), "sample").unwrap();
        // The read hook must equal the written hook structurally — including
        // the `config.code` text we extracted to the .py file.
        assert_eq!(original, read);
    }

    #[test]
    fn round_trip_without_code() {
        let mut hook = sample_hook();
        if let Value::Object(map) = &mut hook.config {
            map.remove("code");
        }
        let dir = TempDir::new().unwrap();
        write_hook(dir.path(), "no-code", &hook).unwrap();
        let read = read_hook(dir.path(), "no-code").unwrap();
        assert_eq!(hook, read);
    }

    #[test]
    fn read_missing_file_errors_with_path() {
        let dir = TempDir::new().unwrap();
        let err = read_hook(dir.path(), "nope").unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("nope.json"), "error should name the path: {msg}");
    }

    #[test]
    fn read_with_unicode_code() {
        let dir = TempDir::new().unwrap();
        let mut hook = sample_hook();
        if let Value::Object(map) = &mut hook.config {
            map.insert(
                "code".to_string(),
                Value::String("# žluťoučký kůň\nprint('ok')".to_string()),
            );
        }
        write_hook(dir.path(), "unicode", &hook).unwrap();
        let read = read_hook(dir.path(), "unicode").unwrap();
        assert_eq!(hook, read);
    }
```

- [ ] **Step 2: Run tests**

Run: `. "$HOME/.cargo/env" && cargo test snapshot::hook::tests`
Expected: 8 tests pass (4 from M1 + 4 new).

- [ ] **Step 3: Commit**

```bash
git add src/snapshot/hook.rs
git commit -m "feat(snapshot): add read_hook + round-trip tests"
```

---

## Task 6: Lockfile schema v2 — add `url` and `content_hash`, with v1→v2 migration

**Files:**
- Modify: `src/state/lockfile.rs`

The M1 reviewer flagged that M3's three-way merge will need `url` (cross-references) and a `content_hash` (drift detection). We add them now and migrate existing v1 lockfiles in place.

- [ ] **Step 1: Update `Lockfile` and `ObjectEntry` types**

Replace the contents of `src/state/lockfile.rs`:

```rust
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::path::Path;

/// Current lockfile schema version.
pub const LOCKFILE_VERSION: u32 = 2;

/// rdc lockfile contents. One file per environment, stored at
/// `.rdc/state/<env>.lock.json`. Records the slug↔ID mapping plus
/// metadata used by future three-way-merge logic.
#[derive(Debug, Serialize, Deserialize, PartialEq, Clone)]
pub struct Lockfile {
    pub version: u32,
    /// Per object-type, a map of slug -> entry.
    pub objects: BTreeMap<String, BTreeMap<String, ObjectEntry>>,
}

/// One row in the lockfile.
#[derive(Debug, Serialize, Deserialize, PartialEq, Clone)]
pub struct ObjectEntry {
    /// Numeric Rossum ID.
    pub id: u64,
    /// Canonical Rossum URL for the object (used by M3+ for cross-reference resolution).
    #[serde(default)]
    pub url: Option<String>,
    /// ISO 8601 server timestamp from `modified_at`, if present.
    #[serde(default)]
    pub modified_at: Option<String>,
    /// Hex-encoded SHA-256 of the snapshot bytes that produced this entry.
    /// Used by M3+ to detect local drift without re-fetching the remote.
    #[serde(default)]
    pub content_hash: Option<String>,
}

impl Default for Lockfile {
    fn default() -> Self {
        Self { version: LOCKFILE_VERSION, objects: BTreeMap::new() }
    }
}

impl Lockfile {
    /// Load a lockfile from disk, returning the default value if the file
    /// does not exist. v1 lockfiles are silently migrated to v2 (the new
    /// fields default to None and will be populated on the next pull).
    pub fn load(path: &Path) -> Result<Self> {
        if !path.exists() {
            return Ok(Self::default());
        }
        let raw = std::fs::read_to_string(path)
            .with_context(|| format!("reading {}", path.display()))?;
        let mut lf: Lockfile = serde_json::from_str(&raw)
            .with_context(|| format!("parsing {}", path.display()))?;

        match lf.version {
            1 => {
                // v1 → v2: same top-level shape, but ObjectEntry's new fields
                // default to None thanks to #[serde(default)]. Just bump the
                // version field; the next pull will populate url and content_hash.
                lf.version = LOCKFILE_VERSION;
            }
            v if v == LOCKFILE_VERSION => {}
            v => {
                anyhow::bail!(
                    "lockfile {} has version {} but this rdc supports {}",
                    path.display(),
                    v,
                    LOCKFILE_VERSION
                );
            }
        }
        Ok(lf)
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        let s = serde_json::to_string_pretty(self)
            .context("serializing lockfile")?;
        crate::snapshot::writer::write_atomic(path, format!("{s}\n").as_bytes())?;
        Ok(())
    }

    pub fn upsert(&mut self, kind: &str, slug: &str, entry: ObjectEntry) {
        self.objects
            .entry(kind.to_string())
            .or_default()
            .insert(slug.to_string(), entry);
    }
}

/// Compute a stable SHA-256 over canonical JSON bytes. Hex-encoded.
pub fn content_hash(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    let digest = hasher.finalize();
    let mut hex = String::with_capacity(64);
    for b in digest {
        use std::fmt::Write;
        write!(&mut hex, "{:02x}", b).expect("writing to String cannot fail");
    }
    hex
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn missing_file_returns_empty() {
        let lf = Lockfile::load(Path::new("/nope.json")).unwrap();
        assert_eq!(lf, Lockfile::default());
        assert_eq!(lf.version, LOCKFILE_VERSION);
    }

    #[test]
    fn round_trip_v2() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("dev.lock.json");
        let mut lf = Lockfile::default();
        lf.upsert(
            "hooks",
            "validator-invoices",
            ObjectEntry {
                id: 1,
                url: Some("https://x.rossum.app/api/v1/hooks/1".to_string()),
                modified_at: Some("2026-04-01T10:00:00Z".to_string()),
                content_hash: Some("a".repeat(64)),
            },
        );
        lf.save(&path).unwrap();
        let loaded = Lockfile::load(&path).unwrap();
        assert_eq!(loaded, lf);
    }

    #[test]
    fn future_version_errors() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("dev.lock.json");
        std::fs::write(&path, r#"{"version":999,"objects":{}}"#).unwrap();
        let err = Lockfile::load(&path).unwrap_err();
        assert!(format!("{err:#}").contains("version"));
    }

    #[test]
    fn v1_lockfile_migrates_to_v2_in_memory() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("dev.lock.json");
        // Hand-write a v1 lockfile (no url, no content_hash).
        std::fs::write(
            &path,
            r#"{
  "version": 1,
  "objects": {
    "hooks": {
      "old-hook": {
        "id": 7,
        "modified_at": "2026-03-01T09:00:00Z"
      }
    }
  }
}
"#,
        )
        .unwrap();

        let lf = Lockfile::load(&path).unwrap();
        assert_eq!(lf.version, LOCKFILE_VERSION);
        let entry = &lf.objects["hooks"]["old-hook"];
        assert_eq!(entry.id, 7);
        assert_eq!(entry.modified_at.as_deref(), Some("2026-03-01T09:00:00Z"));
        assert!(entry.url.is_none());
        assert!(entry.content_hash.is_none());
    }

    #[test]
    fn content_hash_is_deterministic() {
        let h1 = content_hash(b"hello");
        let h2 = content_hash(b"hello");
        assert_eq!(h1, h2);
        assert_eq!(h1.len(), 64);
        assert!(h1.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn content_hash_distinguishes_inputs() {
        assert_ne!(content_hash(b"foo"), content_hash(b"bar"));
    }
}
```

- [ ] **Step 2: Update `cli::pull::run` callers of `ObjectEntry`**

In `src/cli/pull.rs`, the `lockfile.upsert(...)` call constructs an `ObjectEntry`. Update it to include `url` and a placeholder `content_hash` (we'll wire actual hashing in the next task; for now pass `None`):

Find:

```rust
        lockfile.upsert(
            "hooks",
            &slug,
            ObjectEntry {
                id: hook.id,
                modified_at: hook
                    .extra
                    .get("modified_at")
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string()),
            },
        );
```

Replace with:

```rust
        lockfile.upsert(
            "hooks",
            &slug,
            ObjectEntry {
                id: hook.id,
                url: Some(hook.url.clone()),
                modified_at: hook
                    .extra
                    .get("modified_at")
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string()),
                content_hash: None,
            },
        );
```

- [ ] **Step 3: Re-export `content_hash` from `state` module**

Open `src/state/mod.rs` and replace its content with:

```rust
pub mod lockfile;

pub use lockfile::{content_hash, Lockfile, ObjectEntry, LOCKFILE_VERSION};
```

- [ ] **Step 4: Run the full suite**

Run: `. "$HOME/.cargo/env" && cargo test`
Expected: existing 35 tests still pass plus the new lockfile tests pass (so 38 total in lib + same integration count → 38 total).

- [ ] **Step 5: Commit**

```bash
git add src/state/ src/cli/pull.rs
git commit -m "feat(state): bump lockfile schema to v2 (url + content_hash) with v1 migration"
```

---

## Task 7: Extract `cli::pull` into a directory module + per-kind drivers

**Files:**
- Delete: `src/cli/pull.rs`
- Create: `src/cli/pull/mod.rs`
- Create: `src/cli/pull/common.rs`
- Create: `src/cli/pull/hooks.rs`

The current `cli::pull::run` mixes orchestration (load config, resolve token, build client, save lockfile) with kind-specific work (slugify hooks, write hook files, record lockfile entries). M2 needs three kinds; we extract the orchestration so each kind is a small, focused function.

- [ ] **Step 1: Delete the existing `cli/pull.rs`**

Run:

```bash
rm src/cli/pull.rs
```

- [ ] **Step 2: Create the new `cli/pull/mod.rs`**

```rust
use crate::api::RossumClient;
use crate::config::ProjectConfig;
use crate::paths::Paths;
use crate::secrets::resolve_token;
use crate::state::Lockfile;
use anyhow::{anyhow, Context, Result};

mod common;
mod hooks;

pub use common::PullCtx;

pub async fn run(env: &str) -> Result<()> {
    let cwd = std::env::current_dir().context("getting current directory")?;
    let paths = Paths::for_env(&cwd, env);

    let cfg = ProjectConfig::load(&paths.project_config())
        .with_context(|| format!("loading project config from {}", paths.project_config().display()))?;

    let env_cfg = cfg
        .envs
        .get(env)
        .ok_or_else(|| anyhow!("env '{env}' is not defined in rdc.toml"))?;

    let token = resolve_token(&cwd, env)?;
    let client = RossumClient::new(env_cfg.api_base.clone(), token)
        .context("constructing Rossum API client")?;

    let mut lockfile = Lockfile::load(&paths.lockfile())?;
    let mut ctx = PullCtx { paths: &paths, client: &client, lockfile: &mut lockfile };

    let n_hooks = hooks::pull(&mut ctx).await
        .with_context(|| format!("pulling hooks for env '{env}'"))?;

    lockfile.save(&paths.lockfile())?;
    println!("Pulled {n_hooks} hooks from env '{env}'");
    Ok(())
}
```

- [ ] **Step 3: Create `cli/pull/common.rs`**

```rust
use crate::api::RossumClient;
use crate::paths::Paths;
use crate::state::{content_hash, Lockfile, ObjectEntry};

/// Shared state passed through every per-kind pull driver.
pub struct PullCtx<'a> {
    pub paths: &'a Paths,
    pub client: &'a RossumClient,
    pub lockfile: &'a mut Lockfile,
}

/// Compute the content hash of an object's serialized form. The pull drivers
/// hash the JSON bytes they're about to write to disk so the lockfile records
/// what was actually persisted.
pub fn hash_for_lockfile(bytes: &[u8]) -> String {
    content_hash(bytes)
}

/// Record an object in the lockfile under the given kind/slug.
pub fn record_object(
    lockfile: &mut Lockfile,
    kind: &str,
    slug: &str,
    id: u64,
    url: Option<String>,
    modified_at: Option<String>,
    content_hash: Option<String>,
) {
    lockfile.upsert(
        kind,
        slug,
        ObjectEntry { id, url, modified_at, content_hash },
    );
}
```

- [ ] **Step 4: Create `cli/pull/hooks.rs`**

```rust
use super::common::{hash_for_lockfile, record_object, PullCtx};
use crate::slug::slugify_unique;
use crate::snapshot::hook::write_hook;
use anyhow::{Context, Result};
use std::collections::HashSet;

/// Pull all hooks from the env's remote into the local snapshot.
/// Returns the number of hooks pulled.
pub async fn pull(ctx: &mut PullCtx<'_>) -> Result<usize> {
    let hooks = ctx
        .client
        .list_hooks()
        .await
        .context("listing hooks")?;

    std::fs::create_dir_all(ctx.paths.hooks_dir())
        .with_context(|| format!("creating {}", ctx.paths.hooks_dir().display()))?;

    let mut used_slugs: HashSet<String> = HashSet::new();
    for hook in &hooks {
        let slug = slugify_unique(&hook.name, &used_slugs);
        used_slugs.insert(slug.clone());

        write_hook(&ctx.paths.hooks_dir(), &slug, hook)
            .with_context(|| format!("writing hook '{}' to disk", hook.name))?;

        // Hash the JSON we just wrote so the lockfile records it.
        let json_path = ctx.paths.hooks_dir().join(format!("{slug}.json"));
        let bytes = std::fs::read(&json_path)
            .with_context(|| format!("reading just-written {}", json_path.display()))?;
        let hash = hash_for_lockfile(&bytes);

        let modified_at = hook
            .extra
            .get("modified_at")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());

        record_object(
            ctx.lockfile,
            "hooks",
            &slug,
            hook.id,
            Some(hook.url.clone()),
            modified_at,
            Some(hash),
        );
    }

    Ok(hooks.len())
}
```

- [ ] **Step 5: Run the full suite**

Run: `. "$HOME/.cargo/env" && cargo test`
Expected: all tests still pass. The integration test `pull_writes_hook_json_and_py_files` exercises the new path — confirm it still asserts the lockfile contains `validator-invoices` and `sftp-import` (it does).

- [ ] **Step 6: Commit**

```bash
git add src/cli/pull*
git commit -m "refactor(cli): split pull into orchestrator + per-kind drivers"
```

---

## Task 8: `Hook::id` and `Hook::modified_at` accessors

**Files:**
- Modify: `src/model/hook.rs`
- Modify: `src/cli/pull/hooks.rs`

The reviewer flagged `hook.extra.get("modified_at")` as a silent-None footgun if `modified_at` is later promoted to a typed field. Add typed accessors that hide the lookup detail.

- [ ] **Step 1: Add accessors to `Hook`**

Open `src/model/hook.rs`. Locate the `pub struct Hook { ... }` definition. Immediately after the struct definition (before the existing `#[cfg(test)] mod tests { ... }`), add:

```rust
impl Hook {
    /// The server-set `modified_at` timestamp, if present. Currently lives in
    /// the forward-compat `extra` map; this accessor isolates that detail.
    pub fn modified_at(&self) -> Option<&str> {
        self.extra.get("modified_at").and_then(|v| v.as_str())
    }
}
```

- [ ] **Step 2: Add a unit test for the accessor**

Inside the existing `mod tests` block, add this test before the closing `}`:

```rust
    #[test]
    fn modified_at_accessor() {
        let payload = json!({
            "id": 1,
            "url": "https://x/api/v1/hooks/1",
            "name": "T",
            "type": "function",
            "modified_at": "2026-04-01T10:00:00Z"
        });
        let hook: Hook = serde_json::from_value(payload).unwrap();
        assert_eq!(hook.modified_at(), Some("2026-04-01T10:00:00Z"));

        let payload = json!({
            "id": 1,
            "url": "https://x/api/v1/hooks/1",
            "name": "T",
            "type": "function"
        });
        let hook: Hook = serde_json::from_value(payload).unwrap();
        assert_eq!(hook.modified_at(), None);
    }
```

- [ ] **Step 3: Use the accessor in `cli/pull/hooks.rs`**

Open `src/cli/pull/hooks.rs`. Find:

```rust
        let modified_at = hook
            .extra
            .get("modified_at")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());
```

Replace with:

```rust
        let modified_at = hook.modified_at().map(|s| s.to_string());
```

- [ ] **Step 4: Run tests**

Run: `. "$HOME/.cargo/env" && cargo test`
Expected: previous tests still pass + 1 new test passes.

- [ ] **Step 5: Commit**

```bash
git add src/model/hook.rs src/cli/pull/hooks.rs
git commit -m "refactor(model): add Hook::modified_at typed accessor"
```

---

## Task 9: `Organization` model

**Files:**
- Create: `src/model/organization.rs`
- Modify: `src/model/mod.rs`

- [ ] **Step 1: Define the Organization struct + tests**

Create `src/model/organization.rs`:

```rust
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;

/// Rossum organization (one per env). The pull command fetches a single
/// organization per env (the one whose ID is in `rdc.toml`).
#[derive(Debug, Serialize, Deserialize, PartialEq, Clone)]
pub struct Organization {
    pub id: u64,
    pub url: String,
    pub name: String,
    #[serde(flatten)]
    pub extra: BTreeMap<String, Value>,
}

impl Organization {
    pub fn modified_at(&self) -> Option<&str> {
        self.extra.get("modified_at").and_then(|v| v.as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;
    use serde_json::json;

    #[test]
    fn round_trip_preserves_unknown_fields() {
        let payload = json!({
            "id": 285704,
            "url": "https://x.rossum.app/api/v1/organizations/285704",
            "name": "Acme",
            "modified_at": "2026-03-01T08:00:00Z",
            "settings": { "ui_settings": { "language": "en" } },
            "users": ["https://x.rossum.app/api/v1/users/1"]
        });
        let org: Organization = serde_json::from_value(payload.clone()).unwrap();
        assert_eq!(org.id, 285704);
        assert_eq!(org.name, "Acme");
        assert_eq!(org.modified_at(), Some("2026-03-01T08:00:00Z"));
        let round_trip = serde_json::to_value(&org).unwrap();
        assert_eq!(round_trip, payload);
    }

    #[test]
    fn modified_at_absent_returns_none() {
        let payload = json!({
            "id": 1,
            "url": "https://x/api/v1/organizations/1",
            "name": "Min"
        });
        let org: Organization = serde_json::from_value(payload).unwrap();
        assert_eq!(org.modified_at(), None);
    }
}
```

- [ ] **Step 2: Update `src/model/mod.rs`**

Replace its content with:

```rust
pub mod hook;
pub mod organization;
pub mod workspace;

pub use hook::Hook;
pub use organization::Organization;
pub use workspace::Workspace;
```

(Note we're forward-declaring `workspace`. Task 11 creates that file. Until then, `cargo build` will fail. We'll create both before running tests.)

- [ ] **Step 3: Skip running tests until Task 11**

The forward-declaration of `workspace` in `mod.rs` will prevent compile until that module exists. We do NOT commit yet — wait for Task 11.

---

## Task 10: `Workspace` model

**Files:**
- Create: `src/model/workspace.rs`

- [ ] **Step 1: Define the Workspace struct + tests**

Create `src/model/workspace.rs`:

```rust
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;

/// Rossum workspace. Each env has 0..N workspaces, each holding queues.
/// In M2 we only snapshot the workspace metadata; queues are M3.
#[derive(Debug, Serialize, Deserialize, PartialEq, Clone)]
pub struct Workspace {
    pub id: u64,
    pub url: String,
    pub name: String,
    pub organization: String,
    #[serde(default)]
    pub queues: Vec<String>,
    #[serde(flatten)]
    pub extra: BTreeMap<String, Value>,
}

impl Workspace {
    pub fn modified_at(&self) -> Option<&str> {
        self.extra.get("modified_at").and_then(|v| v.as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;
    use serde_json::json;

    #[test]
    fn round_trip_preserves_unknown_fields() {
        let payload = json!({
            "id": 700852,
            "url": "https://x.rossum.app/api/v1/workspaces/700852",
            "name": "Invoices AP",
            "organization": "https://x.rossum.app/api/v1/organizations/285704",
            "queues": ["https://x.rossum.app/api/v1/queues/2137275"],
            "modified_at": "2026-03-15T11:00:00Z",
            "metadata": { "tag": "ap" }
        });
        let ws: Workspace = serde_json::from_value(payload.clone()).unwrap();
        assert_eq!(ws.id, 700852);
        assert_eq!(ws.name, "Invoices AP");
        assert_eq!(ws.organization, "https://x.rossum.app/api/v1/organizations/285704");
        assert_eq!(ws.queues.len(), 1);
        assert_eq!(ws.modified_at(), Some("2026-03-15T11:00:00Z"));
        let round_trip = serde_json::to_value(&ws).unwrap();
        assert_eq!(round_trip, payload);
    }

    #[test]
    fn missing_queues_defaults_to_empty() {
        let payload = json!({
            "id": 1,
            "url": "https://x/api/v1/workspaces/1",
            "name": "Min",
            "organization": "https://x/api/v1/organizations/1"
        });
        let ws: Workspace = serde_json::from_value(payload).unwrap();
        assert!(ws.queues.is_empty());
    }
}
```

- [ ] **Step 2: Run tests**

Run: `. "$HOME/.cargo/env" && cargo test --lib model`
Expected: 5 tests pass (2 hook + 2 organization + 1 from workspace's `missing_queues_defaults_to_empty`, plus 1 from `round_trip_preserves_unknown_fields` which is in two modules so let me re-count):

Actually re-counting: hook 4 (M1) + 1 new (`modified_at_accessor` from Task 8) = 5 hook tests. Organization 2 tests. Workspace 2 tests. Total 9 model tests.

- [ ] **Step 3: Commit (Tasks 9 + 10 together because Task 9 was withholding commit)**

```bash
git add src/model/
git commit -m "feat(model): add Organization and Workspace types"
```

---

## Task 11: API methods for `get_organization` and `list_workspaces`

**Files:**
- Modify: `src/api/mod.rs`
- Modify: `tests/api_hooks.rs` (rename to `tests/api.rs` for clarity)

- [ ] **Step 1: Rename the existing API integration test**

Rename `tests/api_hooks.rs` → `tests/api.rs`:

```bash
git mv tests/api_hooks.rs tests/api.rs
```

- [ ] **Step 2: Add fixture files**

Create `testdata/fixtures/organization.json`:

```json
{
  "id": 285704,
  "url": "https://mock.rossum.app/api/v1/organizations/285704",
  "name": "Acme Test Org",
  "modified_at": "2026-03-01T08:00:00Z",
  "settings": { "ui_settings": { "language": "en" } },
  "users": ["https://mock.rossum.app/api/v1/users/1"]
}
```

Create `testdata/fixtures/workspaces_list.json`:

```json
{
  "pagination": {
    "total": 2,
    "total_pages": 1,
    "next": null,
    "previous": null
  },
  "results": [
    {
      "id": 700852,
      "url": "https://mock.rossum.app/api/v1/workspaces/700852",
      "name": "Invoices AP",
      "organization": "https://mock.rossum.app/api/v1/organizations/285704",
      "queues": ["https://mock.rossum.app/api/v1/queues/100"],
      "modified_at": "2026-03-15T11:00:00Z"
    },
    {
      "id": 743213,
      "url": "https://mock.rossum.app/api/v1/workspaces/743213",
      "name": "Purchase Orders",
      "organization": "https://mock.rossum.app/api/v1/organizations/285704",
      "queues": [],
      "modified_at": "2026-03-15T12:00:00Z"
    }
  ]
}
```

- [ ] **Step 3: Extend the integration test with org + workspaces cases**

Open `tests/api.rs`. Append (after the existing two tests):

```rust
#[tokio::test]
async fn get_organization_returns_org() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/v1/organizations/285704"))
        .and(header("Authorization", "token TEST_TOKEN"))
        .respond_with(ResponseTemplate::new(200).set_body_json(fixture("organization.json")))
        .mount(&server)
        .await;

    let client = RossumClient::new(format!("{}/api/v1", server.uri()), "TEST_TOKEN".into()).unwrap();
    let org = client.get_organization(285704).await.unwrap();
    assert_eq!(org.id, 285704);
    assert_eq!(org.name, "Acme Test Org");
}

#[tokio::test]
async fn list_workspaces_returns_workspaces() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/v1/workspaces"))
        .and(header("Authorization", "token TEST_TOKEN"))
        .respond_with(ResponseTemplate::new(200).set_body_json(fixture("workspaces_list.json")))
        .mount(&server)
        .await;

    let client = RossumClient::new(format!("{}/api/v1", server.uri()), "TEST_TOKEN".into()).unwrap();
    let workspaces = client.list_workspaces().await.unwrap();
    assert_eq!(workspaces.len(), 2);
    assert_eq!(workspaces[0].name, "Invoices AP");
    assert_eq!(workspaces[1].name, "Purchase Orders");
}
```

- [ ] **Step 4: Run the test, verify FAILS**

Run: `. "$HOME/.cargo/env" && cargo test --test api`
Expected: 2 new tests fail (`get_organization` and `list_workspaces` not defined).

- [ ] **Step 5: Implement the new API methods**

Open `src/api/mod.rs`. Locate the `impl RossumClient { ... }` block and add the following methods inside it (after the existing `list_hooks` method, before the `get_json` private helper):

```rust
    pub async fn get_organization(&self, id: u64) -> Result<crate::model::Organization> {
        let url = format!("{}/organizations/{id}", self.base_url);
        self.get_json(&url).await
    }

    pub async fn list_workspaces(&self) -> Result<Vec<crate::model::Workspace>> {
        let mut url = format!("{}/workspaces", self.base_url);
        let mut out = Vec::new();
        loop {
            let page: Page<crate::model::Workspace> = self.get_json(&url).await?;
            out.extend(page.results);
            match page.pagination.next {
                Some(next) => url = next,
                None => break,
            }
        }
        Ok(out)
    }
```

- [ ] **Step 6: Run the test, verify PASS**

Run: `. "$HOME/.cargo/env" && cargo test --test api`
Expected: 4 tests pass (2 from M1 + 2 new).

- [ ] **Step 7: Commit**

```bash
git add src/ tests/ testdata/
git commit -m "feat(api): add get_organization and list_workspaces"
```

---

## Task 12: Snapshot codec for `Organization`

**Files:**
- Create: `src/snapshot/organization.rs`
- Modify: `src/snapshot/mod.rs`

- [ ] **Step 1: Define the codec**

Create `src/snapshot/organization.rs`:

```rust
use crate::model::Organization;
use crate::snapshot::writer::write_atomic;
use anyhow::{Context, Result};
use std::path::Path;

/// Write an organization to disk as a single JSON file at the given path.
/// (The file path is fixed to `<env_root>/organization.json` by the caller;
/// this codec is path-agnostic.)
pub fn write_organization(path: &Path, org: &Organization) -> Result<()> {
    let bytes = serde_json::to_vec_pretty(org)
        .context("serializing organization")?;
    let mut bytes = bytes;
    bytes.push(b'\n');
    write_atomic(path, &bytes)?;
    Ok(())
}

/// Read an organization from disk.
pub fn read_organization(path: &Path) -> Result<Organization> {
    let raw = std::fs::read_to_string(path)
        .with_context(|| format!("reading {}", path.display()))?;
    let org: Organization = serde_json::from_str(&raw)
        .with_context(|| format!("parsing {}", path.display()))?;
    Ok(org)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use tempfile::TempDir;

    fn sample() -> Organization {
        let v = json!({
            "id": 1,
            "url": "https://x/api/v1/organizations/1",
            "name": "Acme",
            "modified_at": "2026-03-01T08:00:00Z"
        });
        serde_json::from_value(v).unwrap()
    }

    #[test]
    fn round_trip() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("organization.json");
        let original = sample();
        write_organization(&path, &original).unwrap();
        let read = read_organization(&path).unwrap();
        assert_eq!(original, read);
    }

    #[test]
    fn writes_pretty_json_with_trailing_newline() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("organization.json");
        write_organization(&path, &sample()).unwrap();
        let raw = std::fs::read_to_string(&path).unwrap();
        assert!(raw.ends_with('\n'));
        assert!(raw.contains("  \"id\": 1"), "expected pretty-printed JSON, got: {raw}");
    }
}
```

- [ ] **Step 2: Wire into `snapshot/mod.rs`**

Replace `src/snapshot/mod.rs` content with:

```rust
pub mod hook;
pub mod organization;
pub mod workspace;
pub mod writer;
```

(Same forward-declaration trick: `workspace` is created in Task 13.)

- [ ] **Step 3: Withhold commit until Task 13**

We can't `cargo test` until `workspace.rs` exists. Continue.

---

## Task 13: Snapshot codec for `Workspace`

**Files:**
- Create: `src/snapshot/workspace.rs`

- [ ] **Step 1: Define the codec**

Create `src/snapshot/workspace.rs`:

```rust
use crate::model::Workspace;
use crate::snapshot::writer::write_atomic;
use anyhow::{Context, Result};
use std::path::Path;

/// Write a workspace's metadata to `<workspace_dir>/workspace.json`.
/// The caller is responsible for `workspace_dir` existing.
pub fn write_workspace(workspace_dir: &Path, ws: &Workspace) -> Result<()> {
    let path = workspace_dir.join("workspace.json");
    let bytes = serde_json::to_vec_pretty(ws)
        .context("serializing workspace")?;
    let mut bytes = bytes;
    bytes.push(b'\n');
    write_atomic(&path, &bytes)?;
    Ok(())
}

/// Read a workspace from disk: loads `<workspace_dir>/workspace.json`.
pub fn read_workspace(workspace_dir: &Path) -> Result<Workspace> {
    let path = workspace_dir.join("workspace.json");
    let raw = std::fs::read_to_string(&path)
        .with_context(|| format!("reading {}", path.display()))?;
    let ws: Workspace = serde_json::from_str(&raw)
        .with_context(|| format!("parsing {}", path.display()))?;
    Ok(ws)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use tempfile::TempDir;

    fn sample() -> Workspace {
        let v = json!({
            "id": 1,
            "url": "https://x/api/v1/workspaces/1",
            "name": "AP",
            "organization": "https://x/api/v1/organizations/1",
            "queues": []
        });
        serde_json::from_value(v).unwrap()
    }

    #[test]
    fn round_trip() {
        let dir = TempDir::new().unwrap();
        std::fs::create_dir_all(dir.path().join("ap")).unwrap();
        let original = sample();
        write_workspace(&dir.path().join("ap"), &original).unwrap();
        let read = read_workspace(&dir.path().join("ap")).unwrap();
        assert_eq!(original, read);
    }

    #[test]
    fn writes_into_workspace_json_inside_dir() {
        let dir = TempDir::new().unwrap();
        std::fs::create_dir_all(dir.path().join("ap")).unwrap();
        write_workspace(&dir.path().join("ap"), &sample()).unwrap();
        assert!(dir.path().join("ap/workspace.json").exists());
    }
}
```

- [ ] **Step 2: Run tests (full suite — first run since Task 9 + 10 + 12 + 13 are interdependent)**

Run: `. "$HOME/.cargo/env" && cargo test`
Expected: all tests pass — 38 lib + 4 api + 2 cli_init + 3 cli_pull + 1 cli_version = 48 total. Confirm by checking the summary lines.

- [ ] **Step 3: Commit Tasks 12 + 13 together**

```bash
git add src/snapshot/
git commit -m "feat(snapshot): organization and workspace codecs"
```

---

## Task 14: Per-kind pull driver for organization

**Files:**
- Create: `src/cli/pull/organization.rs`
- Modify: `src/cli/pull/mod.rs`

- [ ] **Step 1: Implement `pull_organization`**

Create `src/cli/pull/organization.rs`:

```rust
use super::common::{hash_for_lockfile, record_object, PullCtx};
use crate::snapshot::organization::write_organization;
use anyhow::{Context, Result};

/// Pull the env's organization. The org_id comes from the env's config in
/// rdc.toml. Returns 1 on success (one organization per env).
pub async fn pull(ctx: &mut PullCtx<'_>, org_id: u64) -> Result<usize> {
    let org = ctx
        .client
        .get_organization(org_id)
        .await
        .with_context(|| format!("fetching organization {org_id}"))?;

    let path = ctx.paths.organization_file();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }

    write_organization(&path, &org)
        .with_context(|| format!("writing organization to {}", path.display()))?;

    let bytes = std::fs::read(&path)
        .with_context(|| format!("reading just-written {}", path.display()))?;
    let hash = hash_for_lockfile(&bytes);

    record_object(
        ctx.lockfile,
        "organization",
        // Slug for the singleton org is just "self" — there's only one per env,
        // so the slug doesn't appear in the filename. We use a fixed key in the
        // lockfile for symmetry with multi-object kinds.
        "self",
        org.id,
        Some(org.url.clone()),
        org.modified_at().map(|s| s.to_string()),
        Some(hash),
    );

    Ok(1)
}
```

- [ ] **Step 2: Wire into the orchestrator**

Open `src/cli/pull/mod.rs`. Replace the `mod hooks;` line with:

```rust
mod hooks;
mod organization;
```

Then update the `run` function to invoke organization pull before hooks. Replace the body of `run` with:

```rust
pub async fn run(env: &str) -> Result<()> {
    let cwd = std::env::current_dir().context("getting current directory")?;
    let paths = Paths::for_env(&cwd, env);

    let cfg = ProjectConfig::load(&paths.project_config())
        .with_context(|| format!("loading project config from {}", paths.project_config().display()))?;

    let env_cfg = cfg
        .envs
        .get(env)
        .ok_or_else(|| anyhow!("env '{env}' is not defined in rdc.toml"))?;

    let token = resolve_token(&cwd, env)?;
    let client = RossumClient::new(env_cfg.api_base.clone(), token)
        .context("constructing Rossum API client")?;

    let mut lockfile = Lockfile::load(&paths.lockfile())?;
    let mut ctx = PullCtx { paths: &paths, client: &client, lockfile: &mut lockfile };

    let n_orgs = organization::pull(&mut ctx, env_cfg.org_id).await
        .with_context(|| format!("pulling organization for env '{env}'"))?;
    let n_hooks = hooks::pull(&mut ctx).await
        .with_context(|| format!("pulling hooks for env '{env}'"))?;

    lockfile.save(&paths.lockfile())?;
    println!("Pulled {n_orgs} organization, {n_hooks} hooks from env '{env}'");
    Ok(())
}
```

- [ ] **Step 3: Update the integration test to expect the new output and the org file**

Open `tests/cli_pull.rs`. The first integration test currently expects `"Pulled 2 hooks"`. Update it to also stub the organization endpoint and expect the new combined output. Replace the test `pull_writes_hook_json_and_py_files` with:

```rust
#[tokio::test]
async fn pull_writes_organization_and_hook_files() {
    let server = MockServer::start().await;

    Mock::given(method("GET"))
        .and(path("/api/v1/organizations/1"))
        .and(header("Authorization", "token TEST_TOKEN"))
        .respond_with(ResponseTemplate::new(200).set_body_json(fixture("organization.json")))
        .mount(&server)
        .await;

    Mock::given(method("GET"))
        .and(path("/api/v1/hooks"))
        .and(header("Authorization", "token TEST_TOKEN"))
        .respond_with(ResponseTemplate::new(200).set_body_json(fixture("hooks_list.json")))
        .mount(&server)
        .await;

    let project = TempDir::new().unwrap();

    Command::cargo_bin("rdc")
        .unwrap()
        .current_dir(project.path())
        .args([
            "init",
            "--name", "test-pull",
            "--env",
            &format!("dev={}/api/v1:1", server.uri()),
        ])
        .assert()
        .success();

    std::fs::write(
        project.path().join("secrets/dev.secrets.json"),
        r#"{"api_token":"TEST_TOKEN"}"#,
    )
    .unwrap();

    Command::cargo_bin("rdc")
        .unwrap()
        .current_dir(project.path())
        .args(["pull", "dev"])
        .assert()
        .success()
        .stdout(predicate::str::contains("Pulled 1 organization"))
        .stdout(predicate::str::contains("2 hooks"));

    let env_root = project.path().join("envs/dev");
    assert!(env_root.join("organization.json").exists());
    let org_raw = std::fs::read_to_string(env_root.join("organization.json")).unwrap();
    assert!(org_raw.contains("Acme Test Org"));

    let hooks_dir = env_root.join("hooks");
    assert!(hooks_dir.join("validator-invoices.json").exists());
    assert!(hooks_dir.join("sftp-import.json").exists());

    let lf = std::fs::read_to_string(project.path().join(".rdc/state/dev.lock.json")).unwrap();
    assert!(lf.contains("organization"));
    assert!(lf.contains("validator-invoices"));
}
```

- [ ] **Step 4: Run tests**

Run: `. "$HOME/.cargo/env" && cargo test`
Expected: all tests pass.

- [ ] **Step 5: Commit**

```bash
git add src/ tests/
git commit -m "feat(cli): pull organization for each env"
```

---

## Task 15: Per-kind pull driver for workspaces

**Files:**
- Create: `src/cli/pull/workspaces.rs`
- Modify: `src/cli/pull/mod.rs`
- Modify: `tests/cli_pull.rs`

- [ ] **Step 1: Implement `pull_workspaces`**

Create `src/cli/pull/workspaces.rs`:

```rust
use super::common::{hash_for_lockfile, record_object, PullCtx};
use crate::slug::slugify_unique;
use crate::snapshot::workspace::write_workspace;
use anyhow::{Context, Result};
use std::collections::HashSet;

/// Pull all workspaces from the env's remote. Each workspace is written as
/// `envs/<env>/workspaces/<slug>/workspace.json`.
/// Returns the number of workspaces pulled.
pub async fn pull(ctx: &mut PullCtx<'_>) -> Result<usize> {
    let workspaces = ctx
        .client
        .list_workspaces()
        .await
        .context("listing workspaces")?;

    std::fs::create_dir_all(ctx.paths.workspaces_dir())
        .with_context(|| format!("creating {}", ctx.paths.workspaces_dir().display()))?;

    let mut used_slugs: HashSet<String> = HashSet::new();
    for ws in &workspaces {
        let slug = slugify_unique(&ws.name, &used_slugs);
        used_slugs.insert(slug.clone());

        let ws_dir = ctx.paths.workspace_dir(&slug);
        std::fs::create_dir_all(&ws_dir)
            .with_context(|| format!("creating {}", ws_dir.display()))?;

        write_workspace(&ws_dir, ws)
            .with_context(|| format!("writing workspace '{}' to disk", ws.name))?;

        let json_path = ws_dir.join("workspace.json");
        let bytes = std::fs::read(&json_path)
            .with_context(|| format!("reading just-written {}", json_path.display()))?;
        let hash = hash_for_lockfile(&bytes);

        record_object(
            ctx.lockfile,
            "workspaces",
            &slug,
            ws.id,
            Some(ws.url.clone()),
            ws.modified_at().map(|s| s.to_string()),
            Some(hash),
        );
    }

    Ok(workspaces.len())
}
```

- [ ] **Step 2: Wire into the orchestrator**

Open `src/cli/pull/mod.rs`. Update the module declarations to add `workspaces`:

```rust
mod hooks;
mod organization;
mod workspaces;
```

Update `run` to call `workspaces::pull` and amend the summary line. Replace the body of `run` with:

```rust
pub async fn run(env: &str) -> Result<()> {
    let cwd = std::env::current_dir().context("getting current directory")?;
    let paths = Paths::for_env(&cwd, env);

    let cfg = ProjectConfig::load(&paths.project_config())
        .with_context(|| format!("loading project config from {}", paths.project_config().display()))?;

    let env_cfg = cfg
        .envs
        .get(env)
        .ok_or_else(|| anyhow!("env '{env}' is not defined in rdc.toml"))?;

    let token = resolve_token(&cwd, env)?;
    let client = RossumClient::new(env_cfg.api_base.clone(), token)
        .context("constructing Rossum API client")?;

    let mut lockfile = Lockfile::load(&paths.lockfile())?;
    let mut ctx = PullCtx { paths: &paths, client: &client, lockfile: &mut lockfile };

    let n_orgs = organization::pull(&mut ctx, env_cfg.org_id).await
        .with_context(|| format!("pulling organization for env '{env}'"))?;
    let n_workspaces = workspaces::pull(&mut ctx).await
        .with_context(|| format!("pulling workspaces for env '{env}'"))?;
    let n_hooks = hooks::pull(&mut ctx).await
        .with_context(|| format!("pulling hooks for env '{env}'"))?;

    lockfile.save(&paths.lockfile())?;
    println!(
        "Pulled {n_orgs} organization, {n_workspaces} workspaces, {n_hooks} hooks from env '{env}'"
    );
    Ok(())
}
```

- [ ] **Step 3: Update integration test to mock workspaces and assert presence**

Open `tests/cli_pull.rs`. In the `pull_writes_organization_and_hook_files` test, after the existing `Mock::given(method("GET")).and(path("/api/v1/hooks"))...` block, add a new mock for workspaces:

```rust
    Mock::given(method("GET"))
        .and(path("/api/v1/workspaces"))
        .and(header("Authorization", "token TEST_TOKEN"))
        .respond_with(ResponseTemplate::new(200).set_body_json(fixture("workspaces_list.json")))
        .mount(&server)
        .await;
```

Then change the assertion lines (currently `Pulled 1 organization` and `2 hooks`) to also assert workspaces. Update the stdout assertions to:

```rust
        .stdout(predicate::str::contains("Pulled 1 organization"))
        .stdout(predicate::str::contains("2 workspaces"))
        .stdout(predicate::str::contains("2 hooks"));
```

Then add at the end of the test (after the existing `let lf = ...` assertions):

```rust
    let ws_root = env_root.join("workspaces");
    assert!(ws_root.join("invoices-ap/workspace.json").exists());
    assert!(ws_root.join("purchase-orders/workspace.json").exists());

    let ws_raw = std::fs::read_to_string(ws_root.join("invoices-ap/workspace.json")).unwrap();
    assert!(ws_raw.contains("Invoices AP"));

    assert!(lf.contains("workspaces"));
    assert!(lf.contains("invoices-ap"));
```

(The existing `let lf = ...` is already declared above; we're just adding more assertions about it.)

- [ ] **Step 4: Rename the test for accuracy**

Rename the test from `pull_writes_organization_and_hook_files` to `pull_writes_organization_workspaces_and_hook_files`. (Update the `#[tokio::test] fn ...` line.)

- [ ] **Step 5: Run the test, verify FAIL then PASS**

Run: `. "$HOME/.cargo/env" && cargo test --test cli_pull`
Expected: First, FAIL because workspaces aren't being mocked yet. After implementing, all three cli_pull tests pass.

(If you implemented all the wiring before re-running, you'll go straight to PASS — that's fine.)

- [ ] **Step 6: Run the full suite**

Run: `. "$HOME/.cargo/env" && cargo test`
Expected: every test passes.

- [ ] **Step 7: Commit**

```bash
git add src/ tests/
git commit -m "feat(cli): pull workspaces for each env"
```

---

## Task 16: Update README to reflect M2 scope

**Files:**
- Modify: `README.md`

- [ ] **Step 1: Update the Status line and Quick Start**

Open `README.md`. Replace its content with:

````
# rdc

`rdc` (Rossum Deployment as Code) snapshots Rossum.ai configurations to disk for AI-assisted local development and deploys them across environments.

**Status:** M2 (foundations + organization + workspaces). Implements `rdc init`, `rdc pull <env>` for organizations, workspaces, and hooks. See `docs/superpowers/specs/2026-05-06-rdc-design.md` for the full design and `docs/superpowers/plans/` for implementation plans.

## Quick start

```sh
cargo install --path .

mkdir my-rossum-project && cd my-rossum-project
rdc init --name my-project \
  --env dev=https://YOUR-ORG.rossum.app/api/v1:YOUR_ORG_ID

# Provide a token for the dev env:
echo '{"api_token":"YOUR_TOKEN"}' > secrets/dev.secrets.json
# OR: export RDC_TOKEN_DEV=YOUR_TOKEN

rdc pull dev
ls envs/dev/
# organization.json  hooks/  workspaces/
```

## Tests

```sh
cargo test
```
````

- [ ] **Step 2: Commit**

```bash
git add README.md
git commit -m "docs: update README for M2 scope"
```

---

## Self-Review

**Spec coverage check:**

| Spec section | Covered by |
|---|---|
| §5.1 Workspace layout — `envs/<env>/organization.json` | Tasks 9, 12, 14 |
| §5.1 Workspace layout — `envs/<env>/workspaces/<slug>/workspace.json` | Tasks 10, 13, 15 |
| §5.1 Workspace layout — `.rdc/state/<env>.lock.json` schema v2 | Task 6 |
| §5.2 Modules — `paths` | Tasks 2, 3 |
| §5.2 Modules — `model::Organization`, `model::Workspace` | Tasks 9, 10 |
| §5.2 Modules — `snapshot::organization`, `snapshot::workspace` | Tasks 12, 13 |
| §5.2 Modules — `api` (extended with org + workspace methods) | Task 11 |
| §5.2 Modules — `state` (extended with content_hash) | Task 6 |
| §6 CLI — `pull` extended for org + workspaces | Tasks 14, 15 |
| §11 Object scope — Organization (read-only) | Task 14 |
| §11 Object scope — Workspaces | Task 15 |
| §11 Object scope — Queues, Inboxes, Schemas, Formulas | Deferred to M3 |
| §11 Object scope — Rules, Labels, Engines, Engine fields | Deferred to M4 |
| §11 Object scope — Workflows, Workflow steps, Email templates | Deferred to M5 |
| §11 Object scope — MDH | Deferred to M6 |
| §13 Error handling — atomic writes + actionable errors | Inherited from M1; Tasks 12-15 follow the same pattern |

**Placeholder scan:** Reviewed for "TBD", "TODO", "fill in", "similar to" patterns. None present. Every code step shows the actual content; every commit step shows the actual command and message.

**Type consistency check:**
- `Paths::for_env(root, env)` matches in Tasks 2, 3, 7, 14, 15.
- `RossumClient::new(...)` returns `Result<Self>` consistently from Task 4 onward (Tasks 4, 7, 11).
- `ObjectEntry { id, url, modified_at, content_hash }` consistent in Tasks 6, 7, 14, 15.
- `record_object` signature consistent across Tasks 7, 14, 15: `(lockfile, kind, slug, id, url, modified_at, content_hash)`.
- `PullCtx { paths, client, lockfile }` consistent in Tasks 7, 14, 15.
- `Hook::modified_at()`, `Organization::modified_at()`, `Workspace::modified_at()` all return `Option<&str>` (Tasks 8, 9, 10).

**Scope check:** This plan produces one shippable, testable unit (`rdc pull <env>` snapshots organization + workspaces + hooks). It is sized similarly to M1 (~16 tasks vs M1's 15) and stays in TDD-discipline range.

---

## Next milestones

- **M3:** The workspace tree — queues, schemas, inboxes, formulas. Each queue gets `envs/<env>/workspaces/<slug>/queues/<slug>/{queue.json,schema.json,inbox.json,formulas/*.py}`. Schema codec extracts formula field code into separate `.py` files (mirrors hook code extraction).
- **M4:** Org-level objects — rules, labels, engines, engine fields.
- **M5:** Workflow stuff — workflows, workflow steps, email templates.
- **M6:** MDH — dataset metadata + indexes (no row data per spec §11).
- **M7:** Three-way merge for subsequent pulls (consumes the v2 lockfile's `content_hash` and `url` fields).
- **M8:** Conflict resolver TUI; indexer (`_index.md` per env).
- **M9:** `rdc push`.
- **M10:** Overlays.
- **M11:** Mapping wizard, `rdc plan`, `rdc apply`.
- **M12:** Auxiliary commands (`status`, `diff`, `auth`, `repair`).
- **M13:** Distribution.
