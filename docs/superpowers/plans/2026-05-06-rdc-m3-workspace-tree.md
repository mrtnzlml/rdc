# rdc M3 — Workspace Tree (Queues, Schemas, Formulas, Inboxes) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Apply the M2 reviewer's three minor refactors (codec write functions return bytes, resolve `workspace_filter`, content_hash assertion) and extend the snapshot to the queue subtree: `Queue` (nested under workspaces), `Schema` (with formula extraction parallel to hook code extraction), and `Inbox`. After M3, `rdc pull` produces a snapshot that represents a real Rossum implementation — workspaces with queues, each with a schema and optionally an inbox, with formula field code extracted as `.py` files.

**Architecture:** Follow the established model+codec+driver pattern from M1/M2 for each new object type. The novel piece is the schema codec, which mirrors the hook codec's code-extraction trick: schema fields with type `formula` carry a `formula` string property; the codec strips it from the JSON and writes it as a sibling `.py` file under `<queue_dir>/formulas/<field_id>.py`. Queues are pulled flat from the API but written nested under their workspace on disk; the lockfile's URL→slug map (added in M2) makes the workspace lookup possible.

**Tech Stack:** Same as M2 — Rust 2021, clap, reqwest, serde, tokio, wiremock, sha2.

**What this milestone deliberately omits** (deferred to later milestones):
- Rules, labels, engines, engine fields (M4)
- Workflows, workflow steps, email templates (M5)
- MDH dataset metadata + indexes (M6)
- Three-way merge for subsequent pulls (M7+)
- Conflict resolver TUI, indexer, push, plan/apply, overlays, mapping (M7-M11)

**End state of M3, demonstrable manually:**

```
$ rdc pull dev
Pulled 1 organization, 2 workspaces, 3 queues, 3 schemas, 1 inbox, 2 hooks from env 'dev'

$ tree envs/dev -L 5
envs/dev/
├── hooks/
│   ├── validator-invoices.json
│   └── validator-invoices.py
├── organization.json
└── workspaces/
    ├── invoices-ap/
    │   ├── workspace.json
    │   └── queues/
    │       ├── cost-invoices/
    │       │   ├── queue.json
    │       │   ├── schema.json
    │       │   ├── inbox.json
    │       │   └── formulas/
    │       │       └── amount_total.py
    │       └── credit-notes/
    │           ├── queue.json
    │           └── schema.json
    └── purchase-orders/
        ├── workspace.json
        └── queues/
            └── purchase-orders/
                ├── queue.json
                └── schema.json
```

---

## File Structure

| Path | Status | Responsibility |
|---|---|---|
| `src/paths.rs` | Modify | Add `queues_dir(&self, ws_slug)` and `queue_dir(&self, ws_slug, queue_slug)` |
| `src/snapshot/hook.rs` | Modify | `write_hook` returns `Result<Vec<u8>>` (the JSON bytes written) |
| `src/snapshot/organization.rs` | Modify | `write_organization` returns `Result<Vec<u8>>` |
| `src/snapshot/workspace.rs` | Modify | `write_workspace` returns `Result<Vec<u8>>` |
| `src/cli/pull/hooks.rs` | Modify | Hash the bytes returned from `write_hook` instead of re-reading |
| `src/cli/pull/organization.rs` | Modify | Hash the bytes returned from `write_organization` |
| `src/cli/pull/workspaces.rs` | Modify | Hash the bytes; apply `workspace_filter` regex from EnvConfig |
| `src/api/mod.rs` | Modify | Add `list_queues`, `get_inbox`, `get_schema` methods |
| `src/model/queue.rs` | Create | `Queue` struct |
| `src/model/inbox.rs` | Create | `Inbox` struct |
| `src/model/schema.rs` | Create | `Schema` struct (with `content: Vec<Value>` for forward-compat field shapes) |
| `src/model/mod.rs` | Modify | Re-export new types |
| `src/snapshot/queue.rs` | Create | Queue codec |
| `src/snapshot/inbox.rs` | Create | Inbox codec |
| `src/snapshot/schema.rs` | Create | Schema codec with formula field extraction |
| `src/snapshot/mod.rs` | Modify | Declare new submodules |
| `src/cli/pull/queues.rs` | Create | Pull driver: list queues, look up workspace slug from lockfile, write under workspace dir, then pull each queue's schema and optional inbox |
| `src/cli/pull/mod.rs` | Modify | Wire `queues` driver into orchestrator after workspaces |
| `tests/cli_pull.rs` | Modify | Extend integration test for queues + schemas + formulas + inboxes; add content_hash spot-check |
| `testdata/fixtures/queues_list.json` | Create | Paginated queue list response |
| `testdata/fixtures/schema_1.json` | Create | Schema with one formula field and one regular field |
| `testdata/fixtures/schema_2.json` | Create | Schema with no formula fields |
| `testdata/fixtures/inbox_1.json` | Create | Inbox response |
| `tests/api.rs` | Modify | Add tests for `list_queues`, `get_inbox`, `get_schema` |

---

## Task 1: Codec write functions return their bytes

**Files:**
- Modify: `src/snapshot/hook.rs`
- Modify: `src/snapshot/organization.rs`
- Modify: `src/snapshot/workspace.rs`
- Modify: `src/cli/pull/hooks.rs`
- Modify: `src/cli/pull/organization.rs`
- Modify: `src/cli/pull/workspaces.rs`

The M2 reviewer noted that drivers re-read just-written files to compute content_hash. Eliminate the re-read by having codecs return the bytes they wrote.

- [ ] **Step 1: Update `write_hook` signature and body**

In `src/snapshot/hook.rs`, find `pub fn write_hook(dir: &Path, slug: &str, hook: &Hook) -> Result<()>` and replace its return type and final return statement. Final shape of the function:

```rust
pub fn write_hook(dir: &Path, slug: &str, hook: &Hook) -> Result<Vec<u8>> {
    let mut json_value = serde_json::to_value(hook)
        .context("serializing hook to value")?;

    // Extract `config.code` into a sibling .py file.
    let code = json_value
        .get_mut("config")
        .and_then(|c| c.as_object_mut())
        .and_then(|m| m.remove("code"))
        .and_then(|v| match v {
            Value::String(s) => Some(s),
            _ => None,
        });

    let json_path = dir.join(format!("{slug}.json"));
    let json_bytes = serde_json::to_vec_pretty(&json_value)
        .context("serializing hook json")?;
    let mut json_with_newline = json_bytes;
    json_with_newline.push(b'\n');
    write_atomic(&json_path, &json_with_newline)?;

    if let Some(code) = code {
        let py_path = dir.join(format!("{slug}.py"));
        write_atomic(&py_path, code.as_bytes())?;
    }

    Ok(json_with_newline)
}
```

The change: type signature returns `Result<Vec<u8>>`, and the final `Ok(())` becomes `Ok(json_with_newline)` (the JSON bytes are returned).

- [ ] **Step 2: Update `write_organization` signature and body**

In `src/snapshot/organization.rs`, replace the `write_organization` function with:

```rust
pub fn write_organization(path: &Path, org: &Organization) -> Result<Vec<u8>> {
    let bytes = serde_json::to_vec_pretty(org)
        .context("serializing organization")?;
    let mut bytes = bytes;
    bytes.push(b'\n');
    write_atomic(path, &bytes)?;
    Ok(bytes)
}
```

- [ ] **Step 3: Update `write_workspace` signature and body**

In `src/snapshot/workspace.rs`, replace `write_workspace`:

```rust
pub fn write_workspace(workspace_dir: &Path, ws: &Workspace) -> Result<Vec<u8>> {
    let path = workspace_dir.join("workspace.json");
    let bytes = serde_json::to_vec_pretty(ws)
        .context("serializing workspace")?;
    let mut bytes = bytes;
    bytes.push(b'\n');
    write_atomic(&path, &bytes)?;
    Ok(bytes)
}
```

- [ ] **Step 4: Update `cli/pull/hooks.rs` to hash the returned bytes**

In `src/cli/pull/hooks.rs`, replace the body of the `for hook in &hooks` loop to use the returned bytes:

```rust
    let mut used_slugs: HashSet<String> = HashSet::new();
    for hook in &hooks {
        let slug = slugify_unique(&hook.name, &used_slugs);
        used_slugs.insert(slug.clone());

        let bytes = write_hook(&ctx.paths.hooks_dir(), &slug, hook)
            .with_context(|| format!("writing hook '{}' to disk", hook.name))?;
        let hash = hash_for_lockfile(&bytes);

        let modified_at = hook.modified_at().map(|s| s.to_string());

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
```

The change removes the `let json_path = ...; let bytes = std::fs::read(...)` block — bytes come from `write_hook` directly.

- [ ] **Step 5: Update `cli/pull/organization.rs`**

In `src/cli/pull/organization.rs`, replace the section after `write_organization` with:

```rust
    let bytes = write_organization(&path, &org)
        .with_context(|| format!("writing organization to {}", path.display()))?;
    let hash = hash_for_lockfile(&bytes);

    record_object(
        ctx.lockfile,
        "organization",
        "self",
        org.id,
        Some(org.url.clone()),
        org.modified_at().map(|s| s.to_string()),
        Some(hash),
    );

    Ok(1)
}
```

- [ ] **Step 6: Update `cli/pull/workspaces.rs`**

In `src/cli/pull/workspaces.rs`, replace the body of the `for ws in &workspaces` loop:

```rust
    let mut used_slugs: HashSet<String> = HashSet::new();
    for ws in &workspaces {
        let slug = slugify_unique(&ws.name, &used_slugs);
        used_slugs.insert(slug.clone());

        let ws_dir = ctx.paths.workspace_dir(&slug);
        std::fs::create_dir_all(&ws_dir)
            .with_context(|| format!("creating {}", ws_dir.display()))?;

        let bytes = write_workspace(&ws_dir, ws)
            .with_context(|| format!("writing workspace '{}' to disk", ws.name))?;
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
```

- [ ] **Step 7: Run the full suite**

Run: `. "$HOME/.cargo/env" && cargo test`
Expected: all 61 tests still pass.

- [ ] **Step 8: Commit**

```bash
git add src/
git commit -m "refactor(snapshot): codec write functions return JSON bytes for in-memory hashing"
```

---

## Task 2: Apply `workspace_filter` in workspaces driver

**Files:**
- Modify: `src/cli/pull/workspaces.rs`
- Modify: `tests/cli_pull.rs`

The `EnvConfig::workspace_filter: Option<String>` field has been declared since M2 but never applied. Make it work as a regex applied to workspace `name`. If the filter matches the name, the workspace is pulled; otherwise skipped (and its queues will be skipped too in Task 13).

- [ ] **Step 1: Add the `regex` dependency**

In `Cargo.toml`, add `regex = "1"` to `[dependencies]`, alphabetically after `reqwest`. Final block:

```toml
[dependencies]
anyhow = "1"
clap = { version = "4", features = ["derive"] }
regex = "1"
reqwest = { version = "0.12", features = ["json", "rustls-tls"], default-features = false }
serde = { version = "1", features = ["derive"] }
serde_json = "1"
sha2 = "0.10"
thiserror = "1"
tokio = { version = "1", features = ["macros", "rt-multi-thread", "fs"] }
toml = "0.8"
```

(Alphabetical order: `regex` comes before `reqwest`.)

- [ ] **Step 2: Verify build**

Run: `. "$HOME/.cargo/env" && cargo build`
Expected: clean.

- [ ] **Step 3: Update workspaces driver to take EnvConfig and filter**

In `src/cli/pull/workspaces.rs`, change the function signature to accept the env config (so it has access to `workspace_filter`), and skip workspaces whose name does not match the filter when the filter is set.

Replace `src/cli/pull/workspaces.rs` entirely with:

```rust
use super::common::{hash_for_lockfile, record_object, PullCtx};
use crate::config::EnvConfig;
use crate::slug::slugify_unique;
use crate::snapshot::workspace::write_workspace;
use anyhow::{Context, Result};
use regex::Regex;
use std::collections::HashSet;

/// Pull workspaces from the env's remote that match the configured
/// `workspace_filter` (an optional regex applied to `workspace.name`).
/// When the filter is `None`, all workspaces are pulled.
/// Each workspace is written as `envs/<env>/workspaces/<slug>/workspace.json`.
/// Returns the number of workspaces pulled.
pub async fn pull(ctx: &mut PullCtx<'_>, env_cfg: &EnvConfig) -> Result<usize> {
    let workspaces = ctx
        .client
        .list_workspaces()
        .await
        .context("listing workspaces")?;

    let filter = match &env_cfg.workspace_filter {
        Some(pat) => Some(
            Regex::new(pat)
                .with_context(|| format!("compiling workspace_filter regex '{pat}'"))?,
        ),
        None => None,
    };

    std::fs::create_dir_all(ctx.paths.workspaces_dir())
        .with_context(|| format!("creating {}", ctx.paths.workspaces_dir().display()))?;

    let mut used_slugs: HashSet<String> = HashSet::new();
    let mut count = 0usize;
    for ws in &workspaces {
        if let Some(re) = &filter {
            if !re.is_match(&ws.name) {
                continue;
            }
        }

        let slug = slugify_unique(&ws.name, &used_slugs);
        used_slugs.insert(slug.clone());

        let ws_dir = ctx.paths.workspace_dir(&slug);
        std::fs::create_dir_all(&ws_dir)
            .with_context(|| format!("creating {}", ws_dir.display()))?;

        let bytes = write_workspace(&ws_dir, ws)
            .with_context(|| format!("writing workspace '{}' to disk", ws.name))?;
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

        count += 1;
    }

    Ok(count)
}
```

- [ ] **Step 4: Update the orchestrator to pass env_cfg into workspaces::pull**

In `src/cli/pull/mod.rs`, find the `workspaces::pull(&mut ctx).await` call and change it to `workspaces::pull(&mut ctx, env_cfg).await`. The full updated `run` function body is:

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
    let n_workspaces = workspaces::pull(&mut ctx, env_cfg).await
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

- [ ] **Step 5: Run all tests**

Run: `. "$HOME/.cargo/env" && cargo test`
Expected: all tests still pass; the existing integration test does not set `workspace_filter`, so all workspaces are pulled.

- [ ] **Step 6: Add a unit test for the regex filter behavior**

Append the following test to the bottom of `src/cli/pull/workspaces.rs` (inside a new `#[cfg(test)] mod tests { ... }` block):

```rust
#[cfg(test)]
mod tests {
    use regex::Regex;

    #[test]
    fn filter_regex_matches_dev_prefix() {
        let re = Regex::new("^DEV ").unwrap();
        assert!(re.is_match("DEV Workspace"));
        assert!(!re.is_match("PROD Workspace"));
        assert!(!re.is_match("My DEV Workspace"));
    }

    #[test]
    fn filter_regex_with_alternation() {
        let re = Regex::new("(?i)(invoices|orders)").unwrap();
        assert!(re.is_match("Invoices AP"));
        assert!(re.is_match("Purchase Orders"));
        assert!(!re.is_match("HR"));
    }
}
```

These tests verify the regex semantics rather than the full pull flow (which is exercised in integration tests).

- [ ] **Step 7: Run tests**

Run: `. "$HOME/.cargo/env" && cargo test`
Expected: all tests pass; 2 new unit tests added.

- [ ] **Step 8: Commit**

```bash
git add Cargo.toml Cargo.lock src/
git commit -m "feat(cli): apply workspace_filter regex when pulling workspaces"
```

---

## Task 3: Add `paths` accessors for queue subtree

**Files:**
- Modify: `src/paths.rs`

- [ ] **Step 1: Add the new path methods + tests**

In `src/paths.rs`, find the `impl Paths { ... }` block and add the following methods (before the closing `}` of `impl Paths`):

```rust
    /// `<root>/envs/<env>/workspaces/<ws_slug>/queues/`
    pub fn queues_dir(&self, ws_slug: &str) -> PathBuf {
        self.workspace_dir(ws_slug).join("queues")
    }

    /// `<root>/envs/<env>/workspaces/<ws_slug>/queues/<queue_slug>/`
    pub fn queue_dir(&self, ws_slug: &str, queue_slug: &str) -> PathBuf {
        self.queues_dir(ws_slug).join(queue_slug)
    }
```

In the `#[cfg(test)] mod tests { ... }` block, add these tests just before its closing `}`:

```rust
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
```

- [ ] **Step 2: Run tests**

Run: `. "$HOME/.cargo/env" && cargo test --lib paths`
Expected: 10 tests pass (8 from M2 + 2 new).

- [ ] **Step 3: Commit**

```bash
git add src/paths.rs
git commit -m "feat(paths): add queues_dir and queue_dir accessors"
```

---

## Task 4: `Queue` model

**Files:**
- Create: `src/model/queue.rs`

- [ ] **Step 1: Define the Queue struct + tests**

Create `src/model/queue.rs`:

```rust
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;

/// Rossum queue. Each queue belongs to a workspace and carries one schema
/// (and optionally one inbox).
#[derive(Debug, Serialize, Deserialize, PartialEq, Clone)]
pub struct Queue {
    pub id: u64,
    pub url: String,
    pub name: String,
    pub workspace: String,
    pub schema: String,
    /// Optional inbox URL. Many queues do not have an inbox.
    #[serde(default)]
    pub inbox: Option<String>,
    #[serde(flatten)]
    pub extra: BTreeMap<String, Value>,
}

impl Queue {
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
            "id": 2137275,
            "url": "https://x.rossum.app/api/v1/queues/2137275",
            "name": "Cost Invoices (AT)",
            "workspace": "https://x.rossum.app/api/v1/workspaces/700852",
            "schema": "https://x.rossum.app/api/v1/schemas/1824379",
            "inbox": "https://x.rossum.app/api/v1/inboxes/813566",
            "modified_at": "2026-04-10T09:00:00Z",
            "settings": { "default_score_threshold": 0.8 }
        });
        let q: Queue = serde_json::from_value(payload.clone()).unwrap();
        assert_eq!(q.id, 2137275);
        assert_eq!(q.name, "Cost Invoices (AT)");
        assert_eq!(q.inbox.as_deref(), Some("https://x.rossum.app/api/v1/inboxes/813566"));
        let round_trip = serde_json::to_value(&q).unwrap();
        assert_eq!(round_trip, payload);
    }

    #[test]
    fn missing_inbox_defaults_to_none() {
        let payload = json!({
            "id": 1,
            "url": "https://x/api/v1/queues/1",
            "name": "No Inbox",
            "workspace": "https://x/api/v1/workspaces/1",
            "schema": "https://x/api/v1/schemas/1"
        });
        let q: Queue = serde_json::from_value(payload).unwrap();
        assert!(q.inbox.is_none());
    }
}
```

- [ ] **Step 2: Forward-declare in `src/model/mod.rs`**

(We will create `Inbox` and `Schema` in subsequent tasks. Adding all three module declarations now and committing them at the end of Task 6 keeps the module file consistent.)

For now, edit `src/model/mod.rs` to add `pub mod queue;` and `pub use queue::Queue;`. The file should look like:

```rust
pub mod hook;
pub mod inbox;
pub mod organization;
pub mod queue;
pub mod schema;
pub mod workspace;

pub use hook::Hook;
pub use inbox::Inbox;
pub use organization::Organization;
pub use queue::Queue;
pub use schema::Schema;
pub use workspace::Workspace;
```

The build will fail until `inbox` and `schema` files exist (Tasks 5 and 6). We do NOT commit yet.

---

## Task 5: `Inbox` model

**Files:**
- Create: `src/model/inbox.rs`

- [ ] **Step 1: Define the Inbox struct + tests**

Create `src/model/inbox.rs`:

```rust
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;

/// Rossum inbox. Each inbox is attached to one queue (1:1) and provides an
/// email-ingestion endpoint.
#[derive(Debug, Serialize, Deserialize, PartialEq, Clone)]
pub struct Inbox {
    pub id: u64,
    pub url: String,
    pub name: String,
    pub email: String,
    /// URL of the queue this inbox is attached to.
    pub queues: Vec<String>,
    #[serde(flatten)]
    pub extra: BTreeMap<String, Value>,
}

impl Inbox {
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
            "id": 813566,
            "url": "https://x.rossum.app/api/v1/inboxes/813566",
            "name": "Cost Invoices Inbox",
            "email": "cost-invoices@org.rossum.app",
            "queues": ["https://x.rossum.app/api/v1/queues/2137275"],
            "modified_at": "2026-04-10T09:00:00Z",
            "filters": []
        });
        let inbox: Inbox = serde_json::from_value(payload.clone()).unwrap();
        assert_eq!(inbox.id, 813566);
        assert_eq!(inbox.email, "cost-invoices@org.rossum.app");
        assert_eq!(inbox.queues.len(), 1);
        let round_trip = serde_json::to_value(&inbox).unwrap();
        assert_eq!(round_trip, payload);
    }
}
```

(No commit yet — `Schema` is still missing.)

---

## Task 6: `Schema` model

**Files:**
- Create: `src/model/schema.rs`

The schema's `content` array contains heterogeneous field definitions (sections, simple_value, datapoint, formula, …). For round-trip fidelity in M3, model `content` as `Vec<Value>` — opaque per-field, fully forward-compat. The schema codec (Task 12) walks `content` to find formula fields and extract them.

- [ ] **Step 1: Define the Schema struct + tests**

Create `src/model/schema.rs`:

```rust
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;

/// Rossum schema. Each queue has exactly one schema. The `content` array
/// holds the field definitions; we keep them as opaque `Value`s so unknown
/// field types and nested structures round-trip cleanly. The codec walks
/// `content` to extract formula fields' `formula` strings into sibling .py
/// files (mirroring the hook code-extraction pattern).
#[derive(Debug, Serialize, Deserialize, PartialEq, Clone)]
pub struct Schema {
    pub id: u64,
    pub url: String,
    pub name: String,
    pub queues: Vec<String>,
    /// The schema content tree (sections, datapoints, formulas, etc.). Opaque
    /// in the model; the codec walks it.
    pub content: Vec<Value>,
    #[serde(flatten)]
    pub extra: BTreeMap<String, Value>,
}

impl Schema {
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
    fn round_trip_preserves_content() {
        let payload = json!({
            "id": 1824379,
            "url": "https://x.rossum.app/api/v1/schemas/1824379",
            "name": "Cost Invoices Schema",
            "queues": ["https://x.rossum.app/api/v1/queues/2137275"],
            "content": [
                {
                    "category": "section",
                    "id": "header",
                    "label": "Header",
                    "children": [
                        {
                            "category": "datapoint",
                            "id": "invoice_id",
                            "type": "string"
                        },
                        {
                            "category": "datapoint",
                            "id": "amount_total",
                            "type": "number",
                            "formula": "amount_due + amount_tax"
                        }
                    ]
                }
            ],
            "modified_at": "2026-04-10T09:00:00Z"
        });

        let s: Schema = serde_json::from_value(payload.clone()).unwrap();
        assert_eq!(s.id, 1824379);
        assert_eq!(s.content.len(), 1);
        let round_trip = serde_json::to_value(&s).unwrap();
        assert_eq!(round_trip, payload);
    }

    #[test]
    fn empty_content_allowed() {
        let payload = json!({
            "id": 1,
            "url": "https://x/api/v1/schemas/1",
            "name": "Empty",
            "queues": [],
            "content": []
        });
        let s: Schema = serde_json::from_value(payload).unwrap();
        assert!(s.content.is_empty());
    }
}
```

- [ ] **Step 2: Run tests**

Run: `. "$HOME/.cargo/env" && cargo test --lib model`
Expected: model tests pass — Hook (5) + Organization (2) + Workspace (2) + Queue (2) + Inbox (1) + Schema (2) = 14.

- [ ] **Step 3: Commit Tasks 4 + 5 + 6 together**

```bash
git add src/model/
git commit -m "feat(model): add Queue, Inbox, and Schema types"
```

---

## Task 7: API methods — `list_queues`, `get_inbox`, `get_schema`

**Files:**
- Modify: `src/api/mod.rs`
- Modify: `tests/api.rs`
- Create: `testdata/fixtures/queues_list.json`
- Create: `testdata/fixtures/inbox_1.json`
- Create: `testdata/fixtures/schema_1.json`
- Create: `testdata/fixtures/schema_2.json`

- [ ] **Step 1: Create fixture files**

Create `testdata/fixtures/queues_list.json`:

```json
{
  "pagination": {
    "total": 3,
    "total_pages": 1,
    "next": null,
    "previous": null
  },
  "results": [
    {
      "id": 100,
      "url": "https://mock.rossum.app/api/v1/queues/100",
      "name": "Cost Invoices",
      "workspace": "https://mock.rossum.app/api/v1/workspaces/700852",
      "schema": "https://mock.rossum.app/api/v1/schemas/200",
      "inbox": "https://mock.rossum.app/api/v1/inboxes/300",
      "modified_at": "2026-04-10T09:00:00Z"
    },
    {
      "id": 101,
      "url": "https://mock.rossum.app/api/v1/queues/101",
      "name": "Credit Notes",
      "workspace": "https://mock.rossum.app/api/v1/workspaces/700852",
      "schema": "https://mock.rossum.app/api/v1/schemas/201",
      "modified_at": "2026-04-10T09:30:00Z"
    },
    {
      "id": 102,
      "url": "https://mock.rossum.app/api/v1/queues/102",
      "name": "Purchase Orders",
      "workspace": "https://mock.rossum.app/api/v1/workspaces/743213",
      "schema": "https://mock.rossum.app/api/v1/schemas/202",
      "modified_at": "2026-04-10T10:00:00Z"
    }
  ]
}
```

Create `testdata/fixtures/inbox_1.json`:

```json
{
  "id": 300,
  "url": "https://mock.rossum.app/api/v1/inboxes/300",
  "name": "Cost Invoices Inbox",
  "email": "cost-invoices@mock.rossum.app",
  "queues": ["https://mock.rossum.app/api/v1/queues/100"],
  "modified_at": "2026-04-10T09:00:00Z",
  "filters": []
}
```

Create `testdata/fixtures/schema_1.json` (with one formula field for the codec to extract):

```json
{
  "id": 200,
  "url": "https://mock.rossum.app/api/v1/schemas/200",
  "name": "Cost Invoices Schema",
  "queues": ["https://mock.rossum.app/api/v1/queues/100"],
  "content": [
    {
      "category": "section",
      "id": "header",
      "label": "Header",
      "children": [
        {
          "category": "datapoint",
          "id": "invoice_id",
          "type": "string"
        },
        {
          "category": "datapoint",
          "id": "amount_total",
          "type": "number",
          "formula": "amount_due + amount_tax"
        }
      ]
    }
  ],
  "modified_at": "2026-04-10T09:00:00Z"
}
```

Create `testdata/fixtures/schema_2.json` (no formulas):

```json
{
  "id": 201,
  "url": "https://mock.rossum.app/api/v1/schemas/201",
  "name": "Credit Notes Schema",
  "queues": ["https://mock.rossum.app/api/v1/queues/101"],
  "content": [
    {
      "category": "datapoint",
      "id": "credit_id",
      "type": "string"
    }
  ],
  "modified_at": "2026-04-10T09:30:00Z"
}
```

Also create `testdata/fixtures/schema_3.json` (used by the third queue in the integration test):

```json
{
  "id": 202,
  "url": "https://mock.rossum.app/api/v1/schemas/202",
  "name": "Purchase Orders Schema",
  "queues": ["https://mock.rossum.app/api/v1/queues/102"],
  "content": [],
  "modified_at": "2026-04-10T10:00:00Z"
}
```

- [ ] **Step 2: Add the failing tests**

Append to `tests/api.rs`:

```rust
#[tokio::test]
async fn list_queues_returns_queues() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/v1/queues"))
        .and(header("Authorization", "token TEST_TOKEN"))
        .respond_with(ResponseTemplate::new(200).set_body_json(fixture("queues_list.json")))
        .mount(&server)
        .await;

    let client = RossumClient::new(format!("{}/api/v1", server.uri()), "TEST_TOKEN".into()).unwrap();
    let queues = client.list_queues().await.unwrap();
    assert_eq!(queues.len(), 3);
    assert_eq!(queues[0].name, "Cost Invoices");
}

#[tokio::test]
async fn get_inbox_returns_inbox() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/v1/inboxes/300"))
        .respond_with(ResponseTemplate::new(200).set_body_json(fixture("inbox_1.json")))
        .mount(&server)
        .await;
    let client = RossumClient::new(format!("{}/api/v1", server.uri()), "TEST_TOKEN".into()).unwrap();
    let inbox = client.get_inbox(300).await.unwrap();
    assert_eq!(inbox.id, 300);
    assert_eq!(inbox.email, "cost-invoices@mock.rossum.app");
}

#[tokio::test]
async fn get_schema_returns_schema() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/v1/schemas/200"))
        .respond_with(ResponseTemplate::new(200).set_body_json(fixture("schema_1.json")))
        .mount(&server)
        .await;
    let client = RossumClient::new(format!("{}/api/v1", server.uri()), "TEST_TOKEN".into()).unwrap();
    let schema = client.get_schema(200).await.unwrap();
    assert_eq!(schema.id, 200);
    assert_eq!(schema.content.len(), 1);
}
```

- [ ] **Step 3: Run the tests, confirm FAIL**

Run: `. "$HOME/.cargo/env" && cargo test --test api`
Expected: 3 new tests fail (`list_queues`, `get_inbox`, `get_schema` not defined).

- [ ] **Step 4: Implement the API methods**

In `src/api/mod.rs`, add the following methods inside the `impl RossumClient { ... }` block, immediately after `list_workspaces`:

```rust
    pub async fn list_queues(&self) -> Result<Vec<crate::model::Queue>> {
        let mut url = format!("{}/queues", self.base_url);
        let mut out = Vec::new();
        loop {
            let page: Page<crate::model::Queue> = self.get_json(&url).await?;
            out.extend(page.results);
            match page.pagination.next {
                Some(next) => url = next,
                None => break,
            }
        }
        Ok(out)
    }

    pub async fn get_inbox(&self, id: u64) -> Result<crate::model::Inbox> {
        let url = format!("{}/inboxes/{id}", self.base_url);
        self.get_json(&url).await
    }

    pub async fn get_schema(&self, id: u64) -> Result<crate::model::Schema> {
        let url = format!("{}/schemas/{id}", self.base_url);
        self.get_json(&url).await
    }
```

- [ ] **Step 5: Run tests**

Run: `. "$HOME/.cargo/env" && cargo test --test api`
Expected: 7 tests pass (4 from M2 + 3 new).

- [ ] **Step 6: Commit**

```bash
git add src/api/ tests/ testdata/
git commit -m "feat(api): add list_queues, get_inbox, get_schema"
```

---

## Task 8: Snapshot codec for `Queue`

**Files:**
- Create: `src/snapshot/queue.rs`

- [ ] **Step 1: Define the codec + tests**

Create `src/snapshot/queue.rs`:

```rust
use crate::model::Queue;
use crate::snapshot::writer::write_atomic;
use anyhow::{Context, Result};
use std::path::Path;

/// Write a queue's JSON to `<queue_dir>/queue.json`. Returns the bytes written
/// (for content_hash). The caller is responsible for `queue_dir` existing.
pub fn write_queue(queue_dir: &Path, q: &Queue) -> Result<Vec<u8>> {
    let path = queue_dir.join("queue.json");
    let bytes = serde_json::to_vec_pretty(q)
        .context("serializing queue")?;
    let mut bytes = bytes;
    bytes.push(b'\n');
    write_atomic(&path, &bytes)?;
    Ok(bytes)
}

/// Read a queue from disk: loads `<queue_dir>/queue.json`.
pub fn read_queue(queue_dir: &Path) -> Result<Queue> {
    let path = queue_dir.join("queue.json");
    let raw = std::fs::read_to_string(&path)
        .with_context(|| format!("reading {}", path.display()))?;
    let q: Queue = serde_json::from_str(&raw)
        .with_context(|| format!("parsing {}", path.display()))?;
    Ok(q)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use tempfile::TempDir;

    fn sample() -> Queue {
        let v = json!({
            "id": 1,
            "url": "https://x/api/v1/queues/1",
            "name": "Q",
            "workspace": "https://x/api/v1/workspaces/1",
            "schema": "https://x/api/v1/schemas/1"
        });
        serde_json::from_value(v).unwrap()
    }

    #[test]
    fn round_trip() {
        let dir = TempDir::new().unwrap();
        std::fs::create_dir_all(dir.path().join("q1")).unwrap();
        let original = sample();
        write_queue(&dir.path().join("q1"), &original).unwrap();
        let read = read_queue(&dir.path().join("q1")).unwrap();
        assert_eq!(original, read);
    }

    #[test]
    fn writes_into_queue_json_inside_dir() {
        let dir = TempDir::new().unwrap();
        std::fs::create_dir_all(dir.path().join("q1")).unwrap();
        write_queue(&dir.path().join("q1"), &sample()).unwrap();
        assert!(dir.path().join("q1/queue.json").exists());
    }
}
```

(No commit yet — `inbox` and `schema` codecs come next, all wired into `mod.rs` together.)

---

## Task 9: Snapshot codec for `Inbox`

**Files:**
- Create: `src/snapshot/inbox.rs`

- [ ] **Step 1: Define the codec + tests**

Create `src/snapshot/inbox.rs`:

```rust
use crate::model::Inbox;
use crate::snapshot::writer::write_atomic;
use anyhow::{Context, Result};
use std::path::Path;

/// Write an inbox's JSON to `<queue_dir>/inbox.json`. Returns the bytes written.
pub fn write_inbox(queue_dir: &Path, inbox: &Inbox) -> Result<Vec<u8>> {
    let path = queue_dir.join("inbox.json");
    let bytes = serde_json::to_vec_pretty(inbox)
        .context("serializing inbox")?;
    let mut bytes = bytes;
    bytes.push(b'\n');
    write_atomic(&path, &bytes)?;
    Ok(bytes)
}

/// Read an inbox from disk: loads `<queue_dir>/inbox.json`.
pub fn read_inbox(queue_dir: &Path) -> Result<Inbox> {
    let path = queue_dir.join("inbox.json");
    let raw = std::fs::read_to_string(&path)
        .with_context(|| format!("reading {}", path.display()))?;
    let inbox: Inbox = serde_json::from_str(&raw)
        .with_context(|| format!("parsing {}", path.display()))?;
    Ok(inbox)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use tempfile::TempDir;

    fn sample() -> Inbox {
        let v = json!({
            "id": 1,
            "url": "https://x/api/v1/inboxes/1",
            "name": "Inbox",
            "email": "x@mock",
            "queues": ["https://x/api/v1/queues/1"]
        });
        serde_json::from_value(v).unwrap()
    }

    #[test]
    fn round_trip() {
        let dir = TempDir::new().unwrap();
        std::fs::create_dir_all(dir.path().join("q1")).unwrap();
        let original = sample();
        write_inbox(&dir.path().join("q1"), &original).unwrap();
        let read = read_inbox(&dir.path().join("q1")).unwrap();
        assert_eq!(original, read);
    }
}
```

(No commit yet.)

---

## Task 10: Snapshot codec for `Schema` (with formula extraction)

**Files:**
- Create: `src/snapshot/schema.rs`

This is the only novel piece in M3. The codec walks the schema's `content` array recursively. For every leaf object that has `"category": "datapoint"` AND a string `"formula"` property, it:
1. Reads the `id` of the field (the only thing that uniquely identifies it within the schema).
2. Removes `"formula"` from the JSON.
3. Writes the formula text as `<queue_dir>/formulas/<field_id>.py`.

Rules and edge cases:
- The walker recurses into `children` arrays (sections have children; tuples/multivalues sometimes also).
- Field IDs that contain non-slug-safe characters are written as-is into the filename — Rossum field IDs are restricted to `[a-zA-Z0-9_]` by convention, so no slugify needed.
- Two fields with the same ID in the same schema would collide on disk. That's a server data error; we deliberately let the second write overwrite (warn-once would be M5+ territory).
- Read merges formulas back: walks the JSON, for each datapoint node check if a `<id>.py` exists, and if so insert its contents as `formula`.

- [ ] **Step 1: Define the codec + tests**

Create `src/snapshot/schema.rs`:

```rust
use crate::model::Schema;
use crate::snapshot::writer::write_atomic;
use anyhow::{Context, Result};
use serde_json::Value;
use std::path::Path;

/// Write a schema to `<queue_dir>/schema.json`, extracting any formula field
/// `formula` strings into `<queue_dir>/formulas/<field_id>.py` files.
/// Returns the JSON bytes written (for content_hash).
pub fn write_schema(queue_dir: &Path, schema: &Schema) -> Result<Vec<u8>> {
    let mut value = serde_json::to_value(schema)
        .context("serializing schema to value")?;

    // Walk the content tree, extracting formula strings.
    let mut formulas: Vec<(String, String)> = Vec::new();
    if let Some(content) = value.get_mut("content").and_then(|c| c.as_array_mut()) {
        for node in content.iter_mut() {
            extract_formulas(node, &mut formulas);
        }
    }

    let json_path = queue_dir.join("schema.json");
    let bytes = serde_json::to_vec_pretty(&value)
        .context("serializing schema json")?;
    let mut bytes = bytes;
    bytes.push(b'\n');
    write_atomic(&json_path, &bytes)?;

    if !formulas.is_empty() {
        let formulas_dir = queue_dir.join("formulas");
        std::fs::create_dir_all(&formulas_dir)
            .with_context(|| format!("creating {}", formulas_dir.display()))?;
        for (field_id, formula) in formulas {
            let py_path = formulas_dir.join(format!("{field_id}.py"));
            // Byte-exact: write the formula text as-is, no trailing-newline padding.
            write_atomic(&py_path, formula.as_bytes())?;
        }
    }

    Ok(bytes)
}

/// Read a schema from disk. If a `formulas/` subdirectory exists, splice each
/// `<id>.py` back into the corresponding datapoint's `formula` property.
pub fn read_schema(queue_dir: &Path) -> Result<Schema> {
    let json_path = queue_dir.join("schema.json");
    let raw = std::fs::read_to_string(&json_path)
        .with_context(|| format!("reading {}", json_path.display()))?;
    let mut value: Value = serde_json::from_str(&raw)
        .with_context(|| format!("parsing {}", json_path.display()))?;

    let formulas_dir = queue_dir.join("formulas");
    if formulas_dir.is_dir() {
        if let Some(content) = value.get_mut("content").and_then(|c| c.as_array_mut()) {
            for node in content.iter_mut() {
                merge_formulas(node, &formulas_dir)?;
            }
        }
    }

    let schema: Schema = serde_json::from_value(value)
        .with_context(|| format!("deserializing schema from {}", json_path.display()))?;
    Ok(schema)
}

/// Recursively walk a schema content node. For any datapoint with a string
/// `formula`, remove it and append (id, formula) to `out`. Recurses into
/// `children` arrays.
fn extract_formulas(node: &mut Value, out: &mut Vec<(String, String)>) {
    let Some(obj) = node.as_object_mut() else { return };

    let is_datapoint = obj.get("category").and_then(|c| c.as_str()) == Some("datapoint");
    if is_datapoint {
        let id = obj.get("id").and_then(|i| i.as_str()).map(|s| s.to_string());
        if let Some(id) = id {
            if let Some(Value::String(formula)) = obj.remove("formula") {
                out.push((id, formula));
            }
        }
    }

    if let Some(children) = obj.get_mut("children").and_then(|c| c.as_array_mut()) {
        for child in children.iter_mut() {
            extract_formulas(child, out);
        }
    }
}

/// Reverse of `extract_formulas`: for any datapoint without a `formula`
/// property, look up `<formulas_dir>/<id>.py` and insert its contents.
fn merge_formulas(node: &mut Value, formulas_dir: &Path) -> Result<()> {
    let Some(obj) = node.as_object_mut() else { return Ok(()) };

    let is_datapoint = obj.get("category").and_then(|c| c.as_str()) == Some("datapoint");
    if is_datapoint && !obj.contains_key("formula") {
        let id = obj.get("id").and_then(|i| i.as_str()).map(|s| s.to_string());
        if let Some(id) = id {
            let py_path = formulas_dir.join(format!("{id}.py"));
            if py_path.exists() {
                let formula = std::fs::read_to_string(&py_path)
                    .with_context(|| format!("reading {}", py_path.display()))?;
                obj.insert("formula".to_string(), Value::String(formula));
            }
        }
    }

    if let Some(children) = obj.get_mut("children").and_then(|c| c.as_array_mut()) {
        for child in children.iter_mut() {
            merge_formulas(child, formulas_dir)?;
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use tempfile::TempDir;

    fn sample_with_formula() -> Schema {
        let v = json!({
            "id": 1,
            "url": "https://x/api/v1/schemas/1",
            "name": "S",
            "queues": [],
            "content": [
                {
                    "category": "section",
                    "id": "header",
                    "label": "Header",
                    "children": [
                        {
                            "category": "datapoint",
                            "id": "invoice_id",
                            "type": "string"
                        },
                        {
                            "category": "datapoint",
                            "id": "amount_total",
                            "type": "number",
                            "formula": "amount_due + amount_tax"
                        }
                    ]
                }
            ]
        });
        serde_json::from_value(v).unwrap()
    }

    #[test]
    fn writes_schema_json_and_formulas_py() {
        let dir = TempDir::new().unwrap();
        std::fs::create_dir_all(dir.path().join("q1")).unwrap();
        write_schema(&dir.path().join("q1"), &sample_with_formula()).unwrap();
        assert!(dir.path().join("q1/schema.json").exists());
        assert!(dir.path().join("q1/formulas/amount_total.py").exists());
        let f = std::fs::read_to_string(dir.path().join("q1/formulas/amount_total.py")).unwrap();
        assert_eq!(f, "amount_due + amount_tax");
    }

    #[test]
    fn json_does_not_contain_formula_string() {
        let dir = TempDir::new().unwrap();
        std::fs::create_dir_all(dir.path().join("q1")).unwrap();
        write_schema(&dir.path().join("q1"), &sample_with_formula()).unwrap();
        let raw = std::fs::read_to_string(dir.path().join("q1/schema.json")).unwrap();
        assert!(!raw.contains("amount_due + amount_tax"), "formula should be in .py, not .json: {raw}");
        assert!(raw.contains("amount_total"), "field id preserved");
    }

    #[test]
    fn round_trip_preserves_formula() {
        let dir = TempDir::new().unwrap();
        std::fs::create_dir_all(dir.path().join("q1")).unwrap();
        let original = sample_with_formula();
        write_schema(&dir.path().join("q1"), &original).unwrap();
        let read = read_schema(&dir.path().join("q1")).unwrap();
        assert_eq!(original, read);
    }

    #[test]
    fn schema_with_no_formulas_creates_no_formulas_dir() {
        let v = json!({
            "id": 2,
            "url": "https://x/api/v1/schemas/2",
            "name": "Empty",
            "queues": [],
            "content": [
                {
                    "category": "datapoint",
                    "id": "plain",
                    "type": "string"
                }
            ]
        });
        let schema: Schema = serde_json::from_value(v).unwrap();
        let dir = TempDir::new().unwrap();
        std::fs::create_dir_all(dir.path().join("q2")).unwrap();
        write_schema(&dir.path().join("q2"), &schema).unwrap();
        assert!(dir.path().join("q2/schema.json").exists());
        assert!(!dir.path().join("q2/formulas").exists(), "formulas dir should not be created when no formulas exist");
    }

    #[test]
    fn deeply_nested_formula_extracted() {
        let v = json!({
            "id": 3,
            "url": "https://x/api/v1/schemas/3",
            "name": "Nested",
            "queues": [],
            "content": [
                {
                    "category": "section",
                    "id": "outer",
                    "children": [
                        {
                            "category": "section",
                            "id": "inner",
                            "children": [
                                {
                                    "category": "datapoint",
                                    "id": "deep_field",
                                    "formula": "1 + 2"
                                }
                            ]
                        }
                    ]
                }
            ]
        });
        let schema: Schema = serde_json::from_value(v).unwrap();
        let dir = TempDir::new().unwrap();
        std::fs::create_dir_all(dir.path().join("q3")).unwrap();
        write_schema(&dir.path().join("q3"), &schema).unwrap();
        assert!(dir.path().join("q3/formulas/deep_field.py").exists());
    }
}
```

- [ ] **Step 2: Wire all three new codec submodules into `src/snapshot/mod.rs`**

Replace `src/snapshot/mod.rs` with:

```rust
pub mod hook;
pub mod inbox;
pub mod organization;
pub mod queue;
pub mod schema;
pub mod workspace;
pub mod writer;
```

- [ ] **Step 3: Run all tests**

Run: `. "$HOME/.cargo/env" && cargo test --lib snapshot`
Expected: hook (8) + organization (2) + workspace (2) + queue (2) + inbox (1) + schema (5) + writer (3) = 23 snapshot tests pass.

- [ ] **Step 4: Commit Tasks 8 + 9 + 10 together**

```bash
git add src/snapshot/
git commit -m "feat(snapshot): queue, inbox, schema codecs (with formula extraction)"
```

---

## Task 11: Add `find_workspace_slug_for_url` lockfile helper

**Files:**
- Modify: `src/state/lockfile.rs`

The queue driver (Task 13) needs to find which workspace a queue belongs to. It receives `queue.workspace` (URL) and looks up the workspace's slug in the lockfile (which now records URL).

- [ ] **Step 1: Add the helper + test**

In `src/state/lockfile.rs`, add the following method to the `impl Lockfile { ... }` block (just before its closing `}`):

```rust
    /// Find the slug of an object by its URL within a kind.
    /// Returns None if no entry matches.
    pub fn slug_for_url(&self, kind: &str, url: &str) -> Option<&str> {
        let by_kind = self.objects.get(kind)?;
        for (slug, entry) in by_kind.iter() {
            if entry.url.as_deref() == Some(url) {
                return Some(slug.as_str());
            }
        }
        None
    }
```

In the `#[cfg(test)] mod tests { ... }` block, add this test before its closing `}`:

```rust
    #[test]
    fn slug_for_url_finds_match() {
        let mut lf = Lockfile::default();
        lf.upsert(
            "workspaces",
            "invoices-ap",
            ObjectEntry {
                id: 1,
                url: Some("https://x/api/v1/workspaces/1".to_string()),
                modified_at: None,
                content_hash: None,
            },
        );
        assert_eq!(
            lf.slug_for_url("workspaces", "https://x/api/v1/workspaces/1"),
            Some("invoices-ap"),
        );
        assert_eq!(lf.slug_for_url("workspaces", "https://nope"), None);
        assert_eq!(lf.slug_for_url("hooks", "https://x/api/v1/workspaces/1"), None);
    }
```

- [ ] **Step 2: Run tests**

Run: `. "$HOME/.cargo/env" && cargo test --lib state::lockfile`
Expected: 7 tests pass (6 from M2 + 1 new).

- [ ] **Step 3: Commit**

```bash
git add src/state/
git commit -m "feat(state): Lockfile::slug_for_url helper for cross-kind URL lookup"
```

---

## Task 12: Pull driver for queues (with nested schemas, formulas, inboxes)

**Files:**
- Create: `src/cli/pull/queues.rs`
- Modify: `src/cli/pull/mod.rs`

The queue driver does a lot:
1. List all queues.
2. For each queue, look up its workspace slug from the lockfile (using the URL recorded by the workspaces driver). If the workspace was filtered out, skip the queue.
3. Compute a queue slug (deduped per workspace).
4. Create the queue dir.
5. Write the queue JSON.
6. Pull and write the queue's schema (with formula extraction).
7. If the queue has an inbox, pull and write it.
8. Record queue, schema, inbox in the lockfile.

The driver returns a tuple `(n_queues, n_schemas, n_inboxes)` so the orchestrator's summary line can include all three counts.

- [ ] **Step 1: Implement the driver**

Create `src/cli/pull/queues.rs`:

```rust
use super::common::{hash_for_lockfile, record_object, PullCtx};
use crate::slug::slugify_unique;
use crate::snapshot::inbox::write_inbox;
use crate::snapshot::queue::write_queue;
use crate::snapshot::schema::write_schema;
use anyhow::{Context, Result};
use std::collections::{HashMap, HashSet};

/// Counts of objects pulled by the queues driver. Each queue contributes one
/// queue + one schema + 0 or 1 inboxes.
pub struct QueueCounts {
    pub queues: usize,
    pub schemas: usize,
    pub inboxes: usize,
}

/// Pull all queues, plus for each queue: its schema (with formula extraction)
/// and its optional inbox. Queues whose workspace was filtered out (i.e., not
/// present in the lockfile under "workspaces") are skipped silently.
pub async fn pull(ctx: &mut PullCtx<'_>) -> Result<QueueCounts> {
    let queues = ctx
        .client
        .list_queues()
        .await
        .context("listing queues")?;

    // Per-workspace slug pools so queue slugs are unique within a workspace.
    let mut per_ws_used_slugs: HashMap<String, HashSet<String>> = HashMap::new();
    let mut counts = QueueCounts { queues: 0, schemas: 0, inboxes: 0 };

    for q in &queues {
        let ws_slug = match ctx
            .lockfile
            .slug_for_url("workspaces", &q.workspace)
        {
            Some(s) => s.to_string(),
            None => continue, // workspace was filtered out or not yet pulled; skip queue
        };

        let used = per_ws_used_slugs.entry(ws_slug.clone()).or_default();
        let q_slug = slugify_unique(&q.name, used);
        used.insert(q_slug.clone());

        let queue_dir = ctx.paths.queue_dir(&ws_slug, &q_slug);
        std::fs::create_dir_all(&queue_dir)
            .with_context(|| format!("creating {}", queue_dir.display()))?;

        let bytes = write_queue(&queue_dir, q)
            .with_context(|| format!("writing queue '{}' to disk", q.name))?;
        let hash = hash_for_lockfile(&bytes);
        record_object(
            ctx.lockfile,
            "queues",
            &q_slug,
            q.id,
            Some(q.url.clone()),
            q.modified_at().map(|s| s.to_string()),
            Some(hash),
        );
        counts.queues += 1;

        // Pull the queue's schema.
        let schema_id = parse_id_from_url(&q.schema)
            .with_context(|| format!("parsing schema URL '{}' for queue '{}'", q.schema, q.name))?;
        let schema = ctx
            .client
            .get_schema(schema_id)
            .await
            .with_context(|| format!("fetching schema {schema_id} for queue '{}'", q.name))?;
        let schema_bytes = write_schema(&queue_dir, &schema)
            .with_context(|| format!("writing schema for queue '{}'", q.name))?;
        let schema_hash = hash_for_lockfile(&schema_bytes);
        record_object(
            ctx.lockfile,
            "schemas",
            // Schemas don't have their own slug — they are 1:1 with queues.
            // Use the queue slug for symmetry; the file path makes it unambiguous.
            &q_slug,
            schema.id,
            Some(schema.url.clone()),
            schema.modified_at().map(|s| s.to_string()),
            Some(schema_hash),
        );
        counts.schemas += 1;

        // Pull the queue's inbox, if any.
        if let Some(inbox_url) = &q.inbox {
            let inbox_id = parse_id_from_url(inbox_url)
                .with_context(|| format!("parsing inbox URL '{}' for queue '{}'", inbox_url, q.name))?;
            let inbox = ctx
                .client
                .get_inbox(inbox_id)
                .await
                .with_context(|| format!("fetching inbox {inbox_id} for queue '{}'", q.name))?;
            let inbox_bytes = write_inbox(&queue_dir, &inbox)
                .with_context(|| format!("writing inbox for queue '{}'", q.name))?;
            let inbox_hash = hash_for_lockfile(&inbox_bytes);
            record_object(
                ctx.lockfile,
                "inboxes",
                &q_slug,
                inbox.id,
                Some(inbox.url.clone()),
                inbox.modified_at().map(|s| s.to_string()),
                Some(inbox_hash),
            );
            counts.inboxes += 1;
        }
    }

    Ok(counts)
}

/// Parse the trailing numeric ID out of a Rossum API URL, e.g.
/// `https://x.rossum.app/api/v1/schemas/1234` -> `1234`.
fn parse_id_from_url(url: &str) -> Result<u64> {
    let trimmed = url.trim_end_matches('/');
    let last = trimmed
        .rsplit('/')
        .next()
        .ok_or_else(|| anyhow::anyhow!("URL has no path segments: {url}"))?;
    last.parse::<u64>()
        .map_err(|e| anyhow::anyhow!("URL trailing segment '{last}' is not a u64: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_id_basic() {
        assert_eq!(parse_id_from_url("https://x/api/v1/schemas/1234").unwrap(), 1234);
    }

    #[test]
    fn parse_id_with_trailing_slash() {
        assert_eq!(parse_id_from_url("https://x/api/v1/schemas/9/").unwrap(), 9);
    }

    #[test]
    fn parse_id_non_numeric_errors() {
        assert!(parse_id_from_url("https://x/api/v1/schemas/abc").is_err());
    }
}
```

- [ ] **Step 2: Wire into orchestrator**

In `src/cli/pull/mod.rs`, add `mod queues;` to the module declarations and call `queues::pull` after `workspaces::pull`. Replace the `run` function body with:

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
    let n_workspaces = workspaces::pull(&mut ctx, env_cfg).await
        .with_context(|| format!("pulling workspaces for env '{env}'"))?;
    let qc = queues::pull(&mut ctx).await
        .with_context(|| format!("pulling queues for env '{env}'"))?;
    let n_hooks = hooks::pull(&mut ctx).await
        .with_context(|| format!("pulling hooks for env '{env}'"))?;

    lockfile.save(&paths.lockfile())?;
    println!(
        "Pulled {n_orgs} organization, {n_workspaces} workspaces, {} queues, {} schemas, {} inboxes, {n_hooks} hooks from env '{env}'",
        qc.queues, qc.schemas, qc.inboxes
    );
    Ok(())
}
```

Update the `mod` block at the top to include `queues`:

```rust
mod common;
mod hooks;
mod organization;
mod queues;
mod workspaces;
```

- [ ] **Step 3: Run lib tests**

Run: `. "$HOME/.cargo/env" && cargo test --lib`
Expected: lib tests pass (paths 10 + slug 10 + model tests + state tests + secrets tests + snapshot tests + queues tests = compiles cleanly and unit tests pass; `cli_pull` integration tests will fail until Task 13).

- [ ] **Step 4: Commit**

```bash
git add src/cli/
git commit -m "feat(cli): pull queues with nested schemas, formulas, and inboxes"
```

---

## Task 13: Update integration test for queues + schemas + inboxes + content_hash assertion

**Files:**
- Modify: `tests/cli_pull.rs`

The integration test already mocks org/workspaces/hooks. Add mocks for queues, schemas (3 of them), and one inbox, then assert the new file tree and lockfile fields.

- [ ] **Step 1: Replace the first integration test**

In `tests/cli_pull.rs`, replace the test `pull_writes_organization_workspaces_and_hook_files` with a renamed and extended version:

```rust
#[tokio::test]
async fn pull_writes_full_workspace_tree() {
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

    Mock::given(method("GET"))
        .and(path("/api/v1/workspaces"))
        .and(header("Authorization", "token TEST_TOKEN"))
        .respond_with(ResponseTemplate::new(200).set_body_json(fixture("workspaces_list.json")))
        .mount(&server)
        .await;

    Mock::given(method("GET"))
        .and(path("/api/v1/queues"))
        .and(header("Authorization", "token TEST_TOKEN"))
        .respond_with(ResponseTemplate::new(200).set_body_json(fixture("queues_list.json")))
        .mount(&server)
        .await;

    Mock::given(method("GET"))
        .and(path("/api/v1/schemas/200"))
        .respond_with(ResponseTemplate::new(200).set_body_json(fixture("schema_1.json")))
        .mount(&server)
        .await;

    Mock::given(method("GET"))
        .and(path("/api/v1/schemas/201"))
        .respond_with(ResponseTemplate::new(200).set_body_json(fixture("schema_2.json")))
        .mount(&server)
        .await;

    Mock::given(method("GET"))
        .and(path("/api/v1/schemas/202"))
        .respond_with(ResponseTemplate::new(200).set_body_json(fixture("schema_3.json")))
        .mount(&server)
        .await;

    Mock::given(method("GET"))
        .and(path("/api/v1/inboxes/300"))
        .respond_with(ResponseTemplate::new(200).set_body_json(fixture("inbox_1.json")))
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
        .stdout(predicate::str::contains("2 workspaces"))
        .stdout(predicate::str::contains("3 queues"))
        .stdout(predicate::str::contains("3 schemas"))
        .stdout(predicate::str::contains("1 inboxes"))
        .stdout(predicate::str::contains("2 hooks"));

    let env_root = project.path().join("envs/dev");

    // Organization
    assert!(env_root.join("organization.json").exists());

    // Workspaces
    let ws_root = env_root.join("workspaces");
    assert!(ws_root.join("invoices-ap/workspace.json").exists());
    assert!(ws_root.join("purchase-orders/workspace.json").exists());

    // Queues nested under workspaces
    let cost = ws_root.join("invoices-ap/queues/cost-invoices");
    assert!(cost.join("queue.json").exists());
    assert!(cost.join("schema.json").exists());
    assert!(cost.join("inbox.json").exists());
    assert!(cost.join("formulas/amount_total.py").exists());

    let credit = ws_root.join("invoices-ap/queues/credit-notes");
    assert!(credit.join("queue.json").exists());
    assert!(credit.join("schema.json").exists());
    // No inbox for this queue
    assert!(!credit.join("inbox.json").exists());
    // No formulas for this queue
    assert!(!credit.join("formulas").exists());

    let po = ws_root.join("purchase-orders/queues/purchase-orders");
    assert!(po.join("queue.json").exists());
    assert!(po.join("schema.json").exists());

    // Formula content
    let f = std::fs::read_to_string(cost.join("formulas/amount_total.py")).unwrap();
    assert_eq!(f, "amount_due + amount_tax");

    // Schema JSON does NOT contain the formula string
    let schema_raw = std::fs::read_to_string(cost.join("schema.json")).unwrap();
    assert!(!schema_raw.contains("amount_due + amount_tax"));

    // Hooks still pulled
    let hooks_dir = env_root.join("hooks");
    assert!(hooks_dir.join("validator-invoices.json").exists());

    // Lockfile records all kinds with content_hash populated.
    let lf = std::fs::read_to_string(project.path().join(".rdc/state/dev.lock.json")).unwrap();
    assert!(lf.contains("\"organization\""));
    assert!(lf.contains("\"workspaces\""));
    assert!(lf.contains("\"queues\""));
    assert!(lf.contains("\"schemas\""));
    assert!(lf.contains("\"inboxes\""));
    assert!(lf.contains("\"hooks\""));
    assert!(lf.contains("invoices-ap"));
    assert!(lf.contains("cost-invoices"));
    // content_hash is populated (M2 reviewer's recommendation)
    assert!(lf.contains("\"content_hash\""), "lockfile should record content_hash for entries");
    // Hashes are 64-char hex (SHA-256). Spot-check by counting at least one full hash.
    let hash_re = regex::Regex::new(r#""content_hash":\s*"[0-9a-f]{64}""#).unwrap();
    assert!(hash_re.is_match(&lf), "expected at least one 64-char hex content_hash in lockfile");
}
```

The bottom of the test references `regex::Regex`. Add `regex = "1"` to `[dev-dependencies]` if not already there. (We added it to `[dependencies]` in Task 2; `dev-dependencies` will pick that up automatically when test code uses `regex::`. But to be safe, also add it explicitly to `[dev-dependencies]`.)

Update `Cargo.toml`:

```toml
[dev-dependencies]
assert_cmd = "2"
predicates = "3"
pretty_assertions = "1"
regex = "1"
tempfile = "3"
wiremock = "0.6"
tokio = { version = "1", features = ["macros", "rt-multi-thread", "fs", "test-util"] }
```

(`regex` added alphabetically before `tempfile`.)

- [ ] **Step 2: Run all tests**

Run: `. "$HOME/.cargo/env" && cargo test`
Expected: ALL tests pass — full suite (lib + api + cli_init + cli_pull + cli_version) all green.

- [ ] **Step 3: Commit**

```bash
git add Cargo.toml Cargo.lock tests/cli_pull.rs
git commit -m "test(cli): integration test asserts full workspace tree and content_hash"
```

---

## Task 14: README update

**Files:**
- Modify: `README.md`

- [ ] **Step 1: Update Status and Quick Start**

Replace `README.md` with:

````
# rdc

`rdc` (Rossum Deployment as Code) snapshots Rossum.ai configurations to disk for AI-assisted local development and deploys them across environments.

**Status:** M3 (workspace tree). Implements `rdc init`, `rdc pull <env>` for organizations, workspaces (with optional regex filter), queues, schemas (with formula extraction), inboxes, and hooks. See `docs/superpowers/specs/2026-05-06-rdc-design.md` for the full design and `docs/superpowers/plans/` for implementation plans.

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
tree envs/dev -L 5
# envs/dev/
# ├── hooks/
# ├── organization.json
# └── workspaces/
#     └── <workspace>/
#         ├── workspace.json
#         └── queues/
#             └── <queue>/
#                 ├── queue.json
#                 ├── schema.json
#                 ├── inbox.json (if present)
#                 └── formulas/<field_id>.py (one per formula field)
```

## Tests

```sh
cargo test
```
````

- [ ] **Step 2: Commit**

```bash
git add README.md
git commit -m "docs: update README for M3 scope"
```

---

## Self-Review

**Spec coverage check:**

| Spec section | Covered by |
|---|---|
| §5.1 Workspace layout — `envs/<env>/workspaces/<slug>/queues/<slug>/queue.json` | Tasks 4, 8, 12 |
| §5.1 Workspace layout — `inbox.json` | Tasks 5, 9, 12 |
| §5.1 Workspace layout — `schema.json` | Tasks 6, 10, 12 |
| §5.1 Workspace layout — `formulas/<id>.py` | Task 10 (codec), Task 12 (driver) |
| §5.2 Modules — `model::Queue`, `model::Inbox`, `model::Schema` | Tasks 4, 5, 6 |
| §5.2 Modules — `snapshot::queue`, `snapshot::inbox`, `snapshot::schema` | Tasks 8, 9, 10 |
| §5.2 Modules — `api::list_queues`, `api::get_inbox`, `api::get_schema` | Task 7 |
| §5.2 Modules — `paths` (extended) | Task 3 |
| §5.2 Modules — `state` (slug_for_url helper) | Task 11 |
| §6 CLI — `pull` extended | Task 12 |
| §11 Object scope — Queues, Inboxes, Schemas, Formulas | Tasks 4-13 |
| §11 Object scope — Rules, Labels, Engines, Engine fields | Deferred to M4 |
| §11 Object scope — Workflows, Steps, Email templates | Deferred to M5 |
| §11 Object scope — MDH | Deferred to M6 |
| §13 Error handling — actionable errors | All driver tasks use `with_context` consistently |
| M2 reviewer recommendation: write functions return bytes | Task 1 |
| M2 reviewer recommendation: workspace_filter resolved | Task 2 (regex applied) |
| M2 reviewer recommendation: content_hash assertion in test | Task 13 |

**Placeholder scan:** No "TBD", "TODO", "fill in", "similar to" patterns. Every task has explicit code, exact commands, and exact expected outputs.

**Type consistency check:**
- Codec write signatures match: `write_<kind>(...) -> Result<Vec<u8>>` for hook, organization, workspace (Task 1) and queue, inbox, schema (Tasks 8, 9, 10).
- `Queue` field shape (`workspace`, `schema`, optional `inbox`) referenced consistently in Task 4 (model), Task 7 (API fixture), Task 12 (driver).
- `Schema.content: Vec<Value>` consistent in Task 6 (model), Task 10 (codec), Task 12 (driver).
- `Lockfile::slug_for_url(kind, url)` signature consistent in Task 11 (definition) and Task 12 (use).
- `QueueCounts { queues, schemas, inboxes }` defined in Task 12; consumed in Task 12 orchestrator update.
- `paths.queue_dir(ws_slug, q_slug)` consistent in Task 3 (definition) and Task 12 (use).

**Scope check:** This plan produces one shippable, testable unit (`rdc pull <env>` snapshots the full workspace tree). 14 main tasks (with several internal sub-tasks); comparable in size to M1's 15 and M2's 16.

---

## Next milestones

- **M4:** Rules + labels (org-level, attached to queues by URL).
- **M5:** Engines + engine fields.
- **M6:** Workflows + workflow steps + email templates.
- **M7:** MDH dataset metadata + indexes (no row data).
- **M8:** Three-way merge + content_hash drift detection (consumes M2 lockfile fields).
- **M9:** Conflict resolver TUI; indexer (`_index.md` per env).
- **M10:** `rdc push`.
- **M11:** Overlays.
- **M12:** Mapping wizard, `rdc plan`, `rdc apply`.
- **M13:** Auxiliary commands (`status`, `diff`, `auth`, `repair`).
- **M14:** Distribution.
