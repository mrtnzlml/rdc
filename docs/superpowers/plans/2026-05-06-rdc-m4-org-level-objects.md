# rdc M4 — Org-Level Objects (Rules, Labels, Engines, Engine Fields) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Apply the four M3 reviewer recommendations and add the four remaining org-level Rossum object types (Rules, Labels, Engines, Engine Fields). All four follow the hooks pattern (flat layout `envs/<env>/<kind>/<slug>.json`) so M4 is largely pattern replication. After M4, the snapshot covers everything except workflows/email-templates (M5) and MDH (M6).

**Architecture:** Same model+codec+driver+API pattern proven in M1-M3. The reviewer's refactors are applied early so they propagate to the new drivers automatically. No novel architecture; the value is breadth.

**Tech Stack:** Same as M3 — Rust 2021, clap, reqwest, serde, tokio, wiremock, sha2, regex.

**What this milestone deliberately omits** (deferred to later milestones):
- Workflows + workflow steps + email templates (M5)
- MDH dataset metadata + indexes (M6)
- Three-way merge, conflict resolver, push, plan/apply, overlays, mapping (M7+)

**End state of M4:**

```
$ rdc pull dev
Pulled 1 organization, 2 workspaces, 3 queues, 3 schemas, 1 inbox, 2 hooks,
4 rules, 5 labels, 1 engine, 8 engine fields from env 'dev'

$ ls envs/dev/
engines/  engine-fields/  hooks/  labels/  organization.json  rules/  workspaces/
```

---

## File Structure

| Path | Status | Responsibility |
|---|---|---|
| `src/cli/pull/common.rs` | Modify | Add `pluralize(n, singular, plural)`; relocate `parse_id_from_url` here for sharing |
| `src/cli/pull/queues.rs` | Modify | Use shared `parse_id_from_url` from common |
| `src/cli/pull/mod.rs` | Modify | Use `pluralize` in summary line; wire new drivers |
| `src/cli/pull/rules.rs` | Create | Pull driver for rules |
| `src/cli/pull/labels.rs` | Create | Pull driver for labels |
| `src/cli/pull/engines.rs` | Create | Pull driver for engines |
| `src/cli/pull/engine_fields.rs` | Create | Pull driver for engine fields |
| `src/api/mod.rs` | Modify | Add `list_rules`, `list_labels`, `list_engines`, `list_engine_fields` |
| `src/model/rule.rs` | Create | `Rule` struct |
| `src/model/label.rs` | Create | `Label` struct |
| `src/model/engine.rs` | Create | `Engine` struct |
| `src/model/engine_field.rs` | Create | `EngineField` struct |
| `src/model/mod.rs` | Modify | Re-export new types |
| `src/snapshot/rule.rs` | Create | Rule codec |
| `src/snapshot/label.rs` | Create | Label codec |
| `src/snapshot/engine.rs` | Create | Engine codec |
| `src/snapshot/engine_field.rs` | Create | Engine field codec |
| `src/snapshot/schema.rs` | Modify | Add doc comment about content_hash gap |
| `src/snapshot/mod.rs` | Modify | Declare new submodules |
| `tests/cli_pull.rs` | Modify | Extend integration test for the four new kinds; fix "1 inbox" assertion; add workspace_filter integration test |
| `tests/api.rs` | Modify | Add tests for the four new list methods |
| `testdata/fixtures/rules_list.json` | Create | Fixture |
| `testdata/fixtures/labels_list.json` | Create | Fixture |
| `testdata/fixtures/engines_list.json` | Create | Fixture |
| `testdata/fixtures/engine_fields_list.json` | Create | Fixture |

---

## Task 1: `pluralize` utility + relocate `parse_id_from_url`

**Files:**
- Modify: `src/cli/pull/common.rs`
- Modify: `src/cli/pull/queues.rs`

- [ ] **Step 1: Add `pluralize` and `parse_id_from_url` to `common.rs`**

Replace `src/cli/pull/common.rs`:

```rust
use crate::api::RossumClient;
use crate::paths::Paths;
use crate::state::{content_hash, Lockfile, ObjectEntry};
use anyhow::{anyhow, Result};

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

/// Format `"<n> <noun>"` with correct singular/plural agreement.
/// Used by the pull summary line and any future count-aware UX.
pub fn pluralize(n: usize, singular: &str, plural: &str) -> String {
    if n == 1 {
        format!("1 {singular}")
    } else {
        format!("{n} {plural}")
    }
}

/// Parse the trailing numeric ID out of a Rossum API URL, e.g.
/// `https://x.rossum.app/api/v1/schemas/1234` -> `1234`.
pub fn parse_id_from_url(url: &str) -> Result<u64> {
    let trimmed = url.trim_end_matches('/');
    let last = trimmed
        .rsplit('/')
        .next()
        .ok_or_else(|| anyhow!("URL has no path segments: {url}"))?;
    last.parse::<u64>()
        .map_err(|e| anyhow!("URL trailing segment '{last}' is not a u64: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pluralize_singular() {
        assert_eq!(pluralize(1, "hook", "hooks"), "1 hook");
        assert_eq!(pluralize(1, "inbox", "inboxes"), "1 inbox");
    }

    #[test]
    fn pluralize_plural() {
        assert_eq!(pluralize(0, "hook", "hooks"), "0 hooks");
        assert_eq!(pluralize(2, "hook", "hooks"), "2 hooks");
        assert_eq!(pluralize(0, "inbox", "inboxes"), "0 inboxes");
    }

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

- [ ] **Step 2: Remove `parse_id_from_url` from `queues.rs` and use the shared one**

In `src/cli/pull/queues.rs`, find the local `fn parse_id_from_url` definition (with its `#[cfg(test)] mod tests` block) and DELETE both. Update the use statement at the top to import the shared one. The top of the file becomes:

```rust
use super::common::{hash_for_lockfile, parse_id_from_url, record_object, PullCtx};
use crate::slug::slugify_unique;
use crate::snapshot::inbox::write_inbox;
use crate::snapshot::queue::write_queue;
use crate::snapshot::schema::write_schema;
use anyhow::{Context, Result};
use std::collections::{HashMap, HashSet};
```

(The bottom `#[cfg(test)] mod tests` block in queues.rs and the local `parse_id_from_url` definition are removed.)

- [ ] **Step 3: Run tests**

Run: `. "$HOME/.cargo/env" && cargo test`
Expected: 85 tests still pass; the parse_id tests have moved from `cli::pull::queues` to `cli::pull::common`, plus 2 new pluralize tests, so lib unit-test count is +2 = 74.

- [ ] **Step 4: Commit**

```bash
git add src/cli/pull/
git commit -m "refactor(cli): share parse_id_from_url and add pluralize utility"
```

---

## Task 2: Use `pluralize` in the pull summary

**Files:**
- Modify: `src/cli/pull/mod.rs`
- Modify: `tests/cli_pull.rs`

- [ ] **Step 1: Update the summary line**

In `src/cli/pull/mod.rs`, replace the `println!("Pulled ...")` line at the end of `run` with one that uses `pluralize`. Add `use common::pluralize;` (or use `common::pluralize(...)` directly). Final body of `run`:

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
        "Pulled {}, {}, {}, {}, {}, {} from env '{env}'",
        common::pluralize(n_orgs, "organization", "organizations"),
        common::pluralize(n_workspaces, "workspace", "workspaces"),
        common::pluralize(qc.queues, "queue", "queues"),
        common::pluralize(qc.schemas, "schema", "schemas"),
        common::pluralize(qc.inboxes, "inbox", "inboxes"),
        common::pluralize(n_hooks, "hook", "hooks"),
    );
    Ok(())
}
```

(`common` module needs to be visible — change `mod common;` to `pub mod common;` if not already, OR keep `mod common;` and change the call sites to `super::common::pluralize` from the driver files. Cleanest: change `mod common;` to `pub(crate) mod common;` so other drivers can also use `pluralize` without re-export.)

Modify the `mod` declaration at the top:

```rust
mod common;
```

stays as-is — `common::pluralize` works because we're calling it from the same module that declared `common`. Good.

- [ ] **Step 2: Update the cli_pull integration test that asserts "1 inboxes"**

In `tests/cli_pull.rs`, find the line:

```rust
        .stdout(predicate::str::contains("1 inboxes"))
```

Replace with:

```rust
        .stdout(predicate::str::contains("1 inbox"))
```

(There is one inbox in the fixture, so the singular form should now print.)

- [ ] **Step 3: Run tests**

Run: `. "$HOME/.cargo/env" && cargo test`
Expected: all tests still pass; the integration test now asserts the correct singular form.

- [ ] **Step 4: Commit**

```bash
git add src/cli/pull/mod.rs tests/cli_pull.rs
git commit -m "fix(cli): pluralize count nouns correctly in pull summary"
```

---

## Task 3: Document schema content_hash gap

**Files:**
- Modify: `src/snapshot/schema.rs`

- [ ] **Step 1: Add a doc comment**

In `src/snapshot/schema.rs`, find the doc comment immediately above `pub fn write_schema` and replace it with:

```rust
/// Write a schema to `<queue_dir>/schema.json`, extracting any formula field
/// `formula` strings into `<queue_dir>/formulas/<field_id>.py` files.
/// Returns the JSON bytes written (for content_hash).
///
/// **Hash coverage gap:** The returned bytes are the post-extraction
/// `schema.json` content. Changes to extracted `formulas/*.py` files are NOT
/// reflected in the returned hash. M7's three-way merge must therefore
/// recompute schema hashes by combining schema.json bytes with all formula
/// file bytes — using the lockfile content_hash alone will miss formula-only
/// drift.
```

- [ ] **Step 2: Run build**

Run: `. "$HOME/.cargo/env" && cargo build`
Expected: clean.

- [ ] **Step 3: Commit**

```bash
git add src/snapshot/schema.rs
git commit -m "docs(snapshot): note schema content_hash does not cover .py formula files"
```

---

## Task 4: Workspace filter CLI integration test

**Files:**
- Modify: `tests/cli_pull.rs`

- [ ] **Step 1: Add the failing integration test**

Append to `tests/cli_pull.rs` (after the existing tests):

```rust
#[tokio::test]
async fn pull_with_workspace_filter_skips_non_matching() {
    let server = MockServer::start().await;

    Mock::given(method("GET"))
        .and(path("/api/v1/organizations/1"))
        .and(header("Authorization", "token TEST_TOKEN"))
        .respond_with(ResponseTemplate::new(200).set_body_json(fixture("organization.json")))
        .mount(&server)
        .await;

    Mock::given(method("GET"))
        .and(path("/api/v1/hooks"))
        .respond_with(ResponseTemplate::new(200).set_body_json(fixture("hooks_list.json")))
        .mount(&server)
        .await;

    Mock::given(method("GET"))
        .and(path("/api/v1/workspaces"))
        .respond_with(ResponseTemplate::new(200).set_body_json(fixture("workspaces_list.json")))
        .mount(&server)
        .await;

    Mock::given(method("GET"))
        .and(path("/api/v1/queues"))
        .respond_with(ResponseTemplate::new(200).set_body_json(fixture("queues_list.json")))
        .mount(&server)
        .await;

    // Schemas/inbox mocks: include all so any successful pull works; the
    // filtered queues should never reach the schema/inbox calls because they
    // are skipped before that.
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

    // Hand-edit rdc.toml to add workspace_filter that only matches "Invoices AP".
    let cfg_path = project.path().join("rdc.toml");
    let mut cfg = std::fs::read_to_string(&cfg_path).unwrap();
    cfg = cfg.replace("[envs.dev]", "[envs.dev]\nworkspace_filter = \"^Invoices AP$\"");
    std::fs::write(&cfg_path, cfg).unwrap();

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
        // Only one workspace pulled (Invoices AP); two queues belong to it.
        .stdout(predicate::str::contains("1 workspace"))
        .stdout(predicate::str::contains("2 queues"));

    let env_root = project.path().join("envs/dev");
    let ws_root = env_root.join("workspaces");
    assert!(ws_root.join("invoices-ap").is_dir());
    assert!(!ws_root.join("purchase-orders").exists(), "filtered workspace should not be pulled");

    // The Purchase Orders queue (whose workspace was filtered) is skipped.
    let lf = std::fs::read_to_string(project.path().join(".rdc/state/dev.lock.json")).unwrap();
    assert!(lf.contains("invoices-ap"));
    assert!(!lf.contains("purchase-orders"), "queue from filtered workspace should not appear in lockfile");
}
```

- [ ] **Step 2: Run the test**

Run: `. "$HOME/.cargo/env" && cargo test --test cli_pull`
Expected: 4 tests pass (3 existing + 1 new).

- [ ] **Step 3: Commit**

```bash
git add tests/cli_pull.rs
git commit -m "test(cli): integration test for workspace_filter end-to-end"
```

---

## Task 5: `Rule` model

**Files:**
- Create: `src/model/rule.rs`
- Modify: `src/model/mod.rs`

Rules are queue-attached business-logic objects. They have at minimum: id, url, name, queues (URLs).

- [ ] **Step 1: Define the struct + tests**

Create `src/model/rule.rs`:

```rust
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;

/// Rossum rule. Attached to one or more queues; carries business-logic config.
#[derive(Debug, Serialize, Deserialize, PartialEq, Clone)]
pub struct Rule {
    pub id: u64,
    pub url: String,
    pub name: String,
    #[serde(default)]
    pub queues: Vec<String>,
    #[serde(flatten)]
    pub extra: BTreeMap<String, Value>,
}

impl Rule {
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
            "id": 2597,
            "url": "https://x.rossum.app/api/v1/rules/2597",
            "name": "E-invoice Validation Warning",
            "queues": ["https://x.rossum.app/api/v1/queues/2137275"],
            "modified_at": "2026-04-15T08:00:00Z",
            "trigger": "annotation_content",
            "rule_actions": []
        });
        let r: Rule = serde_json::from_value(payload.clone()).unwrap();
        assert_eq!(r.id, 2597);
        assert_eq!(r.name, "E-invoice Validation Warning");
        assert_eq!(r.queues.len(), 1);
        let round_trip = serde_json::to_value(&r).unwrap();
        assert_eq!(round_trip, payload);
    }

    #[test]
    fn missing_queues_defaults_to_empty() {
        let payload = json!({
            "id": 1,
            "url": "https://x/api/v1/rules/1",
            "name": "R"
        });
        let r: Rule = serde_json::from_value(payload).unwrap();
        assert!(r.queues.is_empty());
    }
}
```

(Module wiring happens at the end of Task 8 along with label/engine/engine_field, since they're declared together in `mod.rs`.)

---

## Task 6: `Label` model

**Files:**
- Create: `src/model/label.rs`

- [ ] **Step 1: Define the struct + tests**

Create `src/model/label.rs`:

```rust
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;

/// Rossum label. Categorizes annotations within an organization.
#[derive(Debug, Serialize, Deserialize, PartialEq, Clone)]
pub struct Label {
    pub id: u64,
    pub url: String,
    pub name: String,
    pub organization: String,
    #[serde(flatten)]
    pub extra: BTreeMap<String, Value>,
}

impl Label {
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
            "id": 11,
            "url": "https://x.rossum.app/api/v1/labels/11",
            "name": "Priority High",
            "organization": "https://x.rossum.app/api/v1/organizations/285704",
            "modified_at": "2026-04-15T08:00:00Z",
            "color": "#ff0000"
        });
        let l: Label = serde_json::from_value(payload.clone()).unwrap();
        assert_eq!(l.id, 11);
        assert_eq!(l.name, "Priority High");
        let round_trip = serde_json::to_value(&l).unwrap();
        assert_eq!(round_trip, payload);
    }
}
```

---

## Task 7: `Engine` model

**Files:**
- Create: `src/model/engine.rs`

- [ ] **Step 1: Define the struct + tests**

Create `src/model/engine.rs`:

```rust
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;

/// Rossum engine. Document-extraction model configuration.
#[derive(Debug, Serialize, Deserialize, PartialEq, Clone)]
pub struct Engine {
    pub id: u64,
    pub url: String,
    pub name: String,
    #[serde(flatten)]
    pub extra: BTreeMap<String, Value>,
}

impl Engine {
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
            "id": 401,
            "url": "https://x.rossum.app/api/v1/engines/401",
            "name": "Invoice Engine",
            "modified_at": "2026-04-15T08:00:00Z",
            "type": "extractor",
            "agenda_id": "invoices"
        });
        let e: Engine = serde_json::from_value(payload.clone()).unwrap();
        assert_eq!(e.id, 401);
        assert_eq!(e.name, "Invoice Engine");
        let round_trip = serde_json::to_value(&e).unwrap();
        assert_eq!(round_trip, payload);
    }
}
```

---

## Task 8: `EngineField` model + wire model/mod.rs

**Files:**
- Create: `src/model/engine_field.rs`
- Modify: `src/model/mod.rs`

- [ ] **Step 1: Define the struct + tests**

Create `src/model/engine_field.rs`:

```rust
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;

/// Rossum engine field. Defines a single extractable field on an engine.
#[derive(Debug, Serialize, Deserialize, PartialEq, Clone)]
pub struct EngineField {
    pub id: u64,
    pub url: String,
    pub name: String,
    pub engine: String,
    #[serde(flatten)]
    pub extra: BTreeMap<String, Value>,
}

impl EngineField {
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
            "id": 501,
            "url": "https://x.rossum.app/api/v1/engine_fields/501",
            "name": "Invoice ID",
            "engine": "https://x.rossum.app/api/v1/engines/401",
            "modified_at": "2026-04-15T08:00:00Z",
            "field_type": "string"
        });
        let ef: EngineField = serde_json::from_value(payload.clone()).unwrap();
        assert_eq!(ef.id, 501);
        assert_eq!(ef.engine, "https://x.rossum.app/api/v1/engines/401");
        let round_trip = serde_json::to_value(&ef).unwrap();
        assert_eq!(round_trip, payload);
    }
}
```

- [ ] **Step 2: Wire all four into `model/mod.rs`**

Replace `src/model/mod.rs` with:

```rust
pub mod engine;
pub mod engine_field;
pub mod hook;
pub mod inbox;
pub mod label;
pub mod organization;
pub mod queue;
pub mod rule;
pub mod schema;
pub mod workspace;

pub use engine::Engine;
pub use engine_field::EngineField;
pub use hook::Hook;
pub use inbox::Inbox;
pub use label::Label;
pub use organization::Organization;
pub use queue::Queue;
pub use rule::Rule;
pub use schema::Schema;
pub use workspace::Workspace;
```

- [ ] **Step 3: Run model tests**

Run: `. "$HOME/.cargo/env" && cargo test --lib model`
Expected: model tests pass — Hook (5) + Inbox (1) + Organization (2) + Workspace (2) + Queue (2) + Schema (2) + Rule (2) + Label (1) + Engine (1) + EngineField (1) = 19 model tests.

- [ ] **Step 4: Commit Tasks 5 + 6 + 7 + 8 together**

```bash
git add src/model/
git commit -m "feat(model): add Rule, Label, Engine, and EngineField types"
```

---

## Task 9: API methods for the four new kinds

**Files:**
- Modify: `src/api/mod.rs`
- Modify: `tests/api.rs`
- Create: `testdata/fixtures/rules_list.json`
- Create: `testdata/fixtures/labels_list.json`
- Create: `testdata/fixtures/engines_list.json`
- Create: `testdata/fixtures/engine_fields_list.json`

Note: Rossum's URL convention for engine fields uses `engine_fields` (with underscore). We follow it.

- [ ] **Step 1: Create fixtures**

Create `testdata/fixtures/rules_list.json`:

```json
{
  "pagination": { "total": 1, "total_pages": 1, "next": null, "previous": null },
  "results": [
    {
      "id": 2597,
      "url": "https://mock.rossum.app/api/v1/rules/2597",
      "name": "E-invoice Validation",
      "queues": ["https://mock.rossum.app/api/v1/queues/100"],
      "modified_at": "2026-04-15T08:00:00Z"
    }
  ]
}
```

Create `testdata/fixtures/labels_list.json`:

```json
{
  "pagination": { "total": 2, "total_pages": 1, "next": null, "previous": null },
  "results": [
    {
      "id": 11,
      "url": "https://mock.rossum.app/api/v1/labels/11",
      "name": "Priority High",
      "organization": "https://mock.rossum.app/api/v1/organizations/285704",
      "color": "#ff0000",
      "modified_at": "2026-04-15T08:00:00Z"
    },
    {
      "id": 12,
      "url": "https://mock.rossum.app/api/v1/labels/12",
      "name": "Needs Review",
      "organization": "https://mock.rossum.app/api/v1/organizations/285704",
      "color": "#ffaa00",
      "modified_at": "2026-04-15T08:00:00Z"
    }
  ]
}
```

Create `testdata/fixtures/engines_list.json`:

```json
{
  "pagination": { "total": 1, "total_pages": 1, "next": null, "previous": null },
  "results": [
    {
      "id": 401,
      "url": "https://mock.rossum.app/api/v1/engines/401",
      "name": "Invoice Engine",
      "modified_at": "2026-04-15T08:00:00Z"
    }
  ]
}
```

Create `testdata/fixtures/engine_fields_list.json`:

```json
{
  "pagination": { "total": 2, "total_pages": 1, "next": null, "previous": null },
  "results": [
    {
      "id": 501,
      "url": "https://mock.rossum.app/api/v1/engine_fields/501",
      "name": "Invoice ID",
      "engine": "https://mock.rossum.app/api/v1/engines/401",
      "modified_at": "2026-04-15T08:00:00Z"
    },
    {
      "id": 502,
      "url": "https://mock.rossum.app/api/v1/engine_fields/502",
      "name": "Total Amount",
      "engine": "https://mock.rossum.app/api/v1/engines/401",
      "modified_at": "2026-04-15T08:00:00Z"
    }
  ]
}
```

- [ ] **Step 2: Add failing tests**

Append to `tests/api.rs`:

```rust
#[tokio::test]
async fn list_rules_returns_rules() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/v1/rules"))
        .respond_with(ResponseTemplate::new(200).set_body_json(fixture("rules_list.json")))
        .mount(&server)
        .await;
    let client = RossumClient::new(format!("{}/api/v1", server.uri()), "TEST_TOKEN".into()).unwrap();
    let rules = client.list_rules().await.unwrap();
    assert_eq!(rules.len(), 1);
    assert_eq!(rules[0].name, "E-invoice Validation");
}

#[tokio::test]
async fn list_labels_returns_labels() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/v1/labels"))
        .respond_with(ResponseTemplate::new(200).set_body_json(fixture("labels_list.json")))
        .mount(&server)
        .await;
    let client = RossumClient::new(format!("{}/api/v1", server.uri()), "TEST_TOKEN".into()).unwrap();
    let labels = client.list_labels().await.unwrap();
    assert_eq!(labels.len(), 2);
    assert_eq!(labels[1].name, "Needs Review");
}

#[tokio::test]
async fn list_engines_returns_engines() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/v1/engines"))
        .respond_with(ResponseTemplate::new(200).set_body_json(fixture("engines_list.json")))
        .mount(&server)
        .await;
    let client = RossumClient::new(format!("{}/api/v1", server.uri()), "TEST_TOKEN".into()).unwrap();
    let engines = client.list_engines().await.unwrap();
    assert_eq!(engines.len(), 1);
    assert_eq!(engines[0].name, "Invoice Engine");
}

#[tokio::test]
async fn list_engine_fields_returns_fields() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/v1/engine_fields"))
        .respond_with(ResponseTemplate::new(200).set_body_json(fixture("engine_fields_list.json")))
        .mount(&server)
        .await;
    let client = RossumClient::new(format!("{}/api/v1", server.uri()), "TEST_TOKEN".into()).unwrap();
    let fields = client.list_engine_fields().await.unwrap();
    assert_eq!(fields.len(), 2);
    assert_eq!(fields[0].name, "Invoice ID");
}
```

- [ ] **Step 3: Implement the API methods**

In `src/api/mod.rs`, inside the `impl RossumClient { ... }` block, add four methods immediately after `get_schema`:

```rust
    pub async fn list_rules(&self) -> Result<Vec<crate::model::Rule>> {
        let mut url = format!("{}/rules", self.base_url);
        let mut out = Vec::new();
        loop {
            let page: Page<crate::model::Rule> = self.get_json(&url).await?;
            out.extend(page.results);
            match page.pagination.next {
                Some(next) => url = next,
                None => break,
            }
        }
        Ok(out)
    }

    pub async fn list_labels(&self) -> Result<Vec<crate::model::Label>> {
        let mut url = format!("{}/labels", self.base_url);
        let mut out = Vec::new();
        loop {
            let page: Page<crate::model::Label> = self.get_json(&url).await?;
            out.extend(page.results);
            match page.pagination.next {
                Some(next) => url = next,
                None => break,
            }
        }
        Ok(out)
    }

    pub async fn list_engines(&self) -> Result<Vec<crate::model::Engine>> {
        let mut url = format!("{}/engines", self.base_url);
        let mut out = Vec::new();
        loop {
            let page: Page<crate::model::Engine> = self.get_json(&url).await?;
            out.extend(page.results);
            match page.pagination.next {
                Some(next) => url = next,
                None => break,
            }
        }
        Ok(out)
    }

    pub async fn list_engine_fields(&self) -> Result<Vec<crate::model::EngineField>> {
        let mut url = format!("{}/engine_fields", self.base_url);
        let mut out = Vec::new();
        loop {
            let page: Page<crate::model::EngineField> = self.get_json(&url).await?;
            out.extend(page.results);
            match page.pagination.next {
                Some(next) => url = next,
                None => break,
            }
        }
        Ok(out)
    }
```

- [ ] **Step 4: Run tests**

Run: `. "$HOME/.cargo/env" && cargo test --test api`
Expected: 11 tests pass (7 from M3 + 4 new).

- [ ] **Step 5: Commit**

```bash
git add src/api/ tests/ testdata/
git commit -m "feat(api): add list_rules, list_labels, list_engines, list_engine_fields"
```

---

## Task 10: Snapshot codecs for the four new kinds

**Files:**
- Create: `src/snapshot/rule.rs`
- Create: `src/snapshot/label.rs`
- Create: `src/snapshot/engine.rs`
- Create: `src/snapshot/engine_field.rs`
- Modify: `src/snapshot/mod.rs`

All four codecs follow the hooks pattern (single JSON file per object, slug-based filename). They're so similar I'm putting them in this single task with one commit.

- [ ] **Step 1: Create `src/snapshot/rule.rs`**

```rust
use crate::model::Rule;
use crate::snapshot::writer::write_atomic;
use anyhow::{Context, Result};
use std::path::Path;

/// Write a rule as `<dir>/<slug>.json`. Returns the bytes written.
pub fn write_rule(dir: &Path, slug: &str, r: &Rule) -> Result<Vec<u8>> {
    let path = dir.join(format!("{slug}.json"));
    let bytes = serde_json::to_vec_pretty(r)
        .context("serializing rule")?;
    let mut bytes = bytes;
    bytes.push(b'\n');
    write_atomic(&path, &bytes)?;
    Ok(bytes)
}

pub fn read_rule(dir: &Path, slug: &str) -> Result<Rule> {
    let path = dir.join(format!("{slug}.json"));
    let raw = std::fs::read_to_string(&path)
        .with_context(|| format!("reading {}", path.display()))?;
    serde_json::from_str(&raw)
        .with_context(|| format!("parsing {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use tempfile::TempDir;

    fn sample() -> Rule {
        let v = json!({
            "id": 1,
            "url": "https://x/api/v1/rules/1",
            "name": "R",
            "queues": []
        });
        serde_json::from_value(v).unwrap()
    }

    #[test]
    fn round_trip() {
        let dir = TempDir::new().unwrap();
        let original = sample();
        write_rule(dir.path(), "r", &original).unwrap();
        let read = read_rule(dir.path(), "r").unwrap();
        assert_eq!(original, read);
    }
}
```

- [ ] **Step 2: Create `src/snapshot/label.rs`**

```rust
use crate::model::Label;
use crate::snapshot::writer::write_atomic;
use anyhow::{Context, Result};
use std::path::Path;

/// Write a label as `<dir>/<slug>.json`. Returns the bytes written.
pub fn write_label(dir: &Path, slug: &str, l: &Label) -> Result<Vec<u8>> {
    let path = dir.join(format!("{slug}.json"));
    let bytes = serde_json::to_vec_pretty(l)
        .context("serializing label")?;
    let mut bytes = bytes;
    bytes.push(b'\n');
    write_atomic(&path, &bytes)?;
    Ok(bytes)
}

pub fn read_label(dir: &Path, slug: &str) -> Result<Label> {
    let path = dir.join(format!("{slug}.json"));
    let raw = std::fs::read_to_string(&path)
        .with_context(|| format!("reading {}", path.display()))?;
    serde_json::from_str(&raw)
        .with_context(|| format!("parsing {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use tempfile::TempDir;

    fn sample() -> Label {
        let v = json!({
            "id": 1,
            "url": "https://x/api/v1/labels/1",
            "name": "L",
            "organization": "https://x/api/v1/organizations/1"
        });
        serde_json::from_value(v).unwrap()
    }

    #[test]
    fn round_trip() {
        let dir = TempDir::new().unwrap();
        let original = sample();
        write_label(dir.path(), "l", &original).unwrap();
        let read = read_label(dir.path(), "l").unwrap();
        assert_eq!(original, read);
    }
}
```

- [ ] **Step 3: Create `src/snapshot/engine.rs`**

```rust
use crate::model::Engine;
use crate::snapshot::writer::write_atomic;
use anyhow::{Context, Result};
use std::path::Path;

/// Write an engine as `<dir>/<slug>.json`. Returns the bytes written.
pub fn write_engine(dir: &Path, slug: &str, e: &Engine) -> Result<Vec<u8>> {
    let path = dir.join(format!("{slug}.json"));
    let bytes = serde_json::to_vec_pretty(e)
        .context("serializing engine")?;
    let mut bytes = bytes;
    bytes.push(b'\n');
    write_atomic(&path, &bytes)?;
    Ok(bytes)
}

pub fn read_engine(dir: &Path, slug: &str) -> Result<Engine> {
    let path = dir.join(format!("{slug}.json"));
    let raw = std::fs::read_to_string(&path)
        .with_context(|| format!("reading {}", path.display()))?;
    serde_json::from_str(&raw)
        .with_context(|| format!("parsing {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use tempfile::TempDir;

    fn sample() -> Engine {
        let v = json!({
            "id": 1,
            "url": "https://x/api/v1/engines/1",
            "name": "E"
        });
        serde_json::from_value(v).unwrap()
    }

    #[test]
    fn round_trip() {
        let dir = TempDir::new().unwrap();
        let original = sample();
        write_engine(dir.path(), "e", &original).unwrap();
        let read = read_engine(dir.path(), "e").unwrap();
        assert_eq!(original, read);
    }
}
```

- [ ] **Step 4: Create `src/snapshot/engine_field.rs`**

```rust
use crate::model::EngineField;
use crate::snapshot::writer::write_atomic;
use anyhow::{Context, Result};
use std::path::Path;

/// Write an engine field as `<dir>/<slug>.json`. Returns the bytes written.
pub fn write_engine_field(dir: &Path, slug: &str, f: &EngineField) -> Result<Vec<u8>> {
    let path = dir.join(format!("{slug}.json"));
    let bytes = serde_json::to_vec_pretty(f)
        .context("serializing engine field")?;
    let mut bytes = bytes;
    bytes.push(b'\n');
    write_atomic(&path, &bytes)?;
    Ok(bytes)
}

pub fn read_engine_field(dir: &Path, slug: &str) -> Result<EngineField> {
    let path = dir.join(format!("{slug}.json"));
    let raw = std::fs::read_to_string(&path)
        .with_context(|| format!("reading {}", path.display()))?;
    serde_json::from_str(&raw)
        .with_context(|| format!("parsing {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use tempfile::TempDir;

    fn sample() -> EngineField {
        let v = json!({
            "id": 1,
            "url": "https://x/api/v1/engine_fields/1",
            "name": "F",
            "engine": "https://x/api/v1/engines/1"
        });
        serde_json::from_value(v).unwrap()
    }

    #[test]
    fn round_trip() {
        let dir = TempDir::new().unwrap();
        let original = sample();
        write_engine_field(dir.path(), "f", &original).unwrap();
        let read = read_engine_field(dir.path(), "f").unwrap();
        assert_eq!(original, read);
    }
}
```

- [ ] **Step 5: Wire into `src/snapshot/mod.rs`**

Replace `src/snapshot/mod.rs`:

```rust
pub mod engine;
pub mod engine_field;
pub mod hook;
pub mod inbox;
pub mod label;
pub mod organization;
pub mod queue;
pub mod rule;
pub mod schema;
pub mod workspace;
pub mod writer;
```

- [ ] **Step 6: Run tests**

Run: `. "$HOME/.cargo/env" && cargo test --lib snapshot`
Expected: snapshot tests pass — hook (8) + inbox (1) + organization (2) + queue (2) + schema (5) + workspace (2) + writer (3) + rule (1) + label (1) + engine (1) + engine_field (1) = 27.

- [ ] **Step 7: Commit**

```bash
git add src/snapshot/
git commit -m "feat(snapshot): rule, label, engine, engine_field codecs"
```

---

## Task 11: Pull drivers + add `paths` accessors for the four new kinds

**Files:**
- Modify: `src/paths.rs`
- Create: `src/cli/pull/rules.rs`
- Create: `src/cli/pull/labels.rs`
- Create: `src/cli/pull/engines.rs`
- Create: `src/cli/pull/engine_fields.rs`
- Modify: `src/cli/pull/mod.rs`

- [ ] **Step 1: Add path accessors**

In `src/paths.rs`, add the following methods inside the `impl Paths { ... }` block (before its closing `}`):

```rust
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
```

(Note: directory uses kebab-case `engine-fields` per spec section 11; the model module is `engine_field` per Rust convention.)

In the `#[cfg(test)] mod tests { ... }` block, add tests before its closing `}`:

```rust
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
```

- [ ] **Step 2: Create `src/cli/pull/rules.rs`**

```rust
use super::common::{hash_for_lockfile, record_object, PullCtx};
use crate::slug::slugify_unique;
use crate::snapshot::rule::write_rule;
use anyhow::{Context, Result};
use std::collections::HashSet;

/// Pull all rules. Returns the count.
pub async fn pull(ctx: &mut PullCtx<'_>) -> Result<usize> {
    let rules = ctx.client.list_rules().await.context("listing rules")?;

    std::fs::create_dir_all(ctx.paths.rules_dir())
        .with_context(|| format!("creating {}", ctx.paths.rules_dir().display()))?;

    let mut used: HashSet<String> = HashSet::new();
    for r in &rules {
        let slug = slugify_unique(&r.name, &used);
        used.insert(slug.clone());

        let bytes = write_rule(&ctx.paths.rules_dir(), &slug, r)
            .with_context(|| format!("writing rule '{}' to disk", r.name))?;
        let hash = hash_for_lockfile(&bytes);

        record_object(
            ctx.lockfile,
            "rules",
            &slug,
            r.id,
            Some(r.url.clone()),
            r.modified_at().map(|s| s.to_string()),
            Some(hash),
        );
    }

    Ok(rules.len())
}
```

- [ ] **Step 3: Create `src/cli/pull/labels.rs`**

```rust
use super::common::{hash_for_lockfile, record_object, PullCtx};
use crate::slug::slugify_unique;
use crate::snapshot::label::write_label;
use anyhow::{Context, Result};
use std::collections::HashSet;

/// Pull all labels. Returns the count.
pub async fn pull(ctx: &mut PullCtx<'_>) -> Result<usize> {
    let labels = ctx.client.list_labels().await.context("listing labels")?;

    std::fs::create_dir_all(ctx.paths.labels_dir())
        .with_context(|| format!("creating {}", ctx.paths.labels_dir().display()))?;

    let mut used: HashSet<String> = HashSet::new();
    for l in &labels {
        let slug = slugify_unique(&l.name, &used);
        used.insert(slug.clone());

        let bytes = write_label(&ctx.paths.labels_dir(), &slug, l)
            .with_context(|| format!("writing label '{}' to disk", l.name))?;
        let hash = hash_for_lockfile(&bytes);

        record_object(
            ctx.lockfile,
            "labels",
            &slug,
            l.id,
            Some(l.url.clone()),
            l.modified_at().map(|s| s.to_string()),
            Some(hash),
        );
    }

    Ok(labels.len())
}
```

- [ ] **Step 4: Create `src/cli/pull/engines.rs`**

```rust
use super::common::{hash_for_lockfile, record_object, PullCtx};
use crate::slug::slugify_unique;
use crate::snapshot::engine::write_engine;
use anyhow::{Context, Result};
use std::collections::HashSet;

/// Pull all engines. Returns the count.
pub async fn pull(ctx: &mut PullCtx<'_>) -> Result<usize> {
    let engines = ctx.client.list_engines().await.context("listing engines")?;

    std::fs::create_dir_all(ctx.paths.engines_dir())
        .with_context(|| format!("creating {}", ctx.paths.engines_dir().display()))?;

    let mut used: HashSet<String> = HashSet::new();
    for e in &engines {
        let slug = slugify_unique(&e.name, &used);
        used.insert(slug.clone());

        let bytes = write_engine(&ctx.paths.engines_dir(), &slug, e)
            .with_context(|| format!("writing engine '{}' to disk", e.name))?;
        let hash = hash_for_lockfile(&bytes);

        record_object(
            ctx.lockfile,
            "engines",
            &slug,
            e.id,
            Some(e.url.clone()),
            e.modified_at().map(|s| s.to_string()),
            Some(hash),
        );
    }

    Ok(engines.len())
}
```

- [ ] **Step 5: Create `src/cli/pull/engine_fields.rs`**

```rust
use super::common::{hash_for_lockfile, record_object, PullCtx};
use crate::slug::slugify_unique;
use crate::snapshot::engine_field::write_engine_field;
use anyhow::{Context, Result};
use std::collections::HashSet;

/// Pull all engine fields. Returns the count.
pub async fn pull(ctx: &mut PullCtx<'_>) -> Result<usize> {
    let fields = ctx
        .client
        .list_engine_fields()
        .await
        .context("listing engine fields")?;

    std::fs::create_dir_all(ctx.paths.engine_fields_dir())
        .with_context(|| format!("creating {}", ctx.paths.engine_fields_dir().display()))?;

    let mut used: HashSet<String> = HashSet::new();
    for f in &fields {
        let slug = slugify_unique(&f.name, &used);
        used.insert(slug.clone());

        let bytes = write_engine_field(&ctx.paths.engine_fields_dir(), &slug, f)
            .with_context(|| format!("writing engine field '{}' to disk", f.name))?;
        let hash = hash_for_lockfile(&bytes);

        record_object(
            ctx.lockfile,
            "engine_fields",
            &slug,
            f.id,
            Some(f.url.clone()),
            f.modified_at().map(|s| s.to_string()),
            Some(hash),
        );
    }

    Ok(fields.len())
}
```

- [ ] **Step 6: Wire all four into `src/cli/pull/mod.rs`**

Replace the `mod` declarations and the call sequence in `run`:

```rust
use crate::api::RossumClient;
use crate::config::ProjectConfig;
use crate::paths::Paths;
use crate::secrets::resolve_token;
use crate::state::Lockfile;
use anyhow::{anyhow, Context, Result};

mod common;
mod engine_fields;
mod engines;
mod hooks;
mod labels;
mod organization;
mod queues;
mod rules;
mod workspaces;

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

    let n_orgs = organization::pull(&mut ctx, env_cfg.org_id).await
        .with_context(|| format!("pulling organization for env '{env}'"))?;
    let n_workspaces = workspaces::pull(&mut ctx, env_cfg).await
        .with_context(|| format!("pulling workspaces for env '{env}'"))?;
    let qc = queues::pull(&mut ctx).await
        .with_context(|| format!("pulling queues for env '{env}'"))?;
    let n_hooks = hooks::pull(&mut ctx).await
        .with_context(|| format!("pulling hooks for env '{env}'"))?;
    let n_rules = rules::pull(&mut ctx).await
        .with_context(|| format!("pulling rules for env '{env}'"))?;
    let n_labels = labels::pull(&mut ctx).await
        .with_context(|| format!("pulling labels for env '{env}'"))?;
    let n_engines = engines::pull(&mut ctx).await
        .with_context(|| format!("pulling engines for env '{env}'"))?;
    let n_engine_fields = engine_fields::pull(&mut ctx).await
        .with_context(|| format!("pulling engine fields for env '{env}'"))?;

    lockfile.save(&paths.lockfile())?;
    println!(
        "Pulled {}, {}, {}, {}, {}, {}, {}, {}, {}, {} from env '{env}'",
        common::pluralize(n_orgs, "organization", "organizations"),
        common::pluralize(n_workspaces, "workspace", "workspaces"),
        common::pluralize(qc.queues, "queue", "queues"),
        common::pluralize(qc.schemas, "schema", "schemas"),
        common::pluralize(qc.inboxes, "inbox", "inboxes"),
        common::pluralize(n_hooks, "hook", "hooks"),
        common::pluralize(n_rules, "rule", "rules"),
        common::pluralize(n_labels, "label", "labels"),
        common::pluralize(n_engines, "engine", "engines"),
        common::pluralize(n_engine_fields, "engine field", "engine fields"),
    );
    Ok(())
}
```

- [ ] **Step 7: Run lib tests**

Run: `. "$HOME/.cargo/env" && cargo test --lib`
Expected: all unit tests pass.

- [ ] **Step 8: Commit**

```bash
git add src/paths.rs src/cli/pull/
git commit -m "feat(cli): pull rules, labels, engines, engine_fields"
```

---

## Task 12: Update integration test for the four new kinds

**Files:**
- Modify: `tests/cli_pull.rs`

- [ ] **Step 1: Update the `pull_writes_full_workspace_tree` test**

Inside that test, add four new mocks (after the existing inbox mock):

```rust
    Mock::given(method("GET"))
        .and(path("/api/v1/rules"))
        .respond_with(ResponseTemplate::new(200).set_body_json(fixture("rules_list.json")))
        .mount(&server)
        .await;

    Mock::given(method("GET"))
        .and(path("/api/v1/labels"))
        .respond_with(ResponseTemplate::new(200).set_body_json(fixture("labels_list.json")))
        .mount(&server)
        .await;

    Mock::given(method("GET"))
        .and(path("/api/v1/engines"))
        .respond_with(ResponseTemplate::new(200).set_body_json(fixture("engines_list.json")))
        .mount(&server)
        .await;

    Mock::given(method("GET"))
        .and(path("/api/v1/engine_fields"))
        .respond_with(ResponseTemplate::new(200).set_body_json(fixture("engine_fields_list.json")))
        .mount(&server)
        .await;
```

Update the stdout assertions block — replace:

```rust
        .stdout(predicate::str::contains("Pulled 1 organization"))
        .stdout(predicate::str::contains("2 workspaces"))
        .stdout(predicate::str::contains("3 queues"))
        .stdout(predicate::str::contains("3 schemas"))
        .stdout(predicate::str::contains("1 inbox"))
        .stdout(predicate::str::contains("2 hooks"));
```

With:

```rust
        .stdout(predicate::str::contains("Pulled 1 organization"))
        .stdout(predicate::str::contains("2 workspaces"))
        .stdout(predicate::str::contains("3 queues"))
        .stdout(predicate::str::contains("3 schemas"))
        .stdout(predicate::str::contains("1 inbox"))
        .stdout(predicate::str::contains("2 hooks"))
        .stdout(predicate::str::contains("1 rule"))
        .stdout(predicate::str::contains("2 labels"))
        .stdout(predicate::str::contains("1 engine"))
        .stdout(predicate::str::contains("2 engine fields"));
```

After the existing assertions (after the `let lf = ...` block), add:

```rust
    // New M4 kinds present
    assert!(env_root.join("rules/e-invoice-validation.json").exists());
    assert!(env_root.join("labels/priority-high.json").exists());
    assert!(env_root.join("labels/needs-review.json").exists());
    assert!(env_root.join("engines/invoice-engine.json").exists());
    assert!(env_root.join("engine-fields/invoice-id.json").exists());
    assert!(env_root.join("engine-fields/total-amount.json").exists());

    // Lockfile records new kinds
    assert!(lf.contains("\"rules\""));
    assert!(lf.contains("\"labels\""));
    assert!(lf.contains("\"engines\""));
    assert!(lf.contains("\"engine_fields\""));
```

Also update the workspace_filter test (Task 4 added) — it needs the four new mocks too. Add the same four `Mock::given(method("GET")).and(path("/api/v1/<kind>"))` blocks (with the appropriate fixture) inside `pull_with_workspace_filter_skips_non_matching` after the inbox mock. Without these, the test fails when the new kinds' API calls go unmocked.

- [ ] **Step 2: Run all tests**

Run: `. "$HOME/.cargo/env" && cargo test`
Expected: ALL tests pass.

- [ ] **Step 3: Commit**

```bash
git add tests/cli_pull.rs
git commit -m "test(cli): integration test for rules, labels, engines, engine_fields"
```

---

## Task 13: README update

**Files:**
- Modify: `README.md`

- [ ] **Step 1: Update Status and Quick Start**

Replace `README.md`:

````
# rdc

`rdc` (Rossum Deployment as Code) snapshots Rossum.ai configurations to disk for AI-assisted local development and deploys them across environments.

**Status:** M4. `rdc init` and `rdc pull <env>` cover organizations, workspaces (with optional regex filter), queues, schemas (with formula extraction), inboxes, hooks, rules, labels, engines, and engine fields. See `docs/superpowers/specs/2026-05-06-rdc-design.md` for the full design and `docs/superpowers/plans/` for implementation plans.

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
# engines/  engine-fields/  hooks/  labels/  organization.json  rules/  workspaces/
```

## Tests

```sh
cargo test
```
````

- [ ] **Step 2: Commit**

```bash
git add README.md
git commit -m "docs: update README for M4 scope"
```

---

## Self-Review

**Spec coverage check:**

| Spec section | Covered by |
|---|---|
| §11 — Rules | Tasks 5, 9, 10, 11, 12 |
| §11 — Labels | Tasks 6, 9, 10, 11, 12 |
| §11 — Engines | Tasks 7, 9, 10, 11, 12 |
| §11 — Engine fields | Tasks 8, 9, 10, 11, 12 |
| §11 — Workflows, steps, email templates | Deferred to M5 |
| §11 — MDH | Deferred to M6 |
| M3 reviewer recommendation: pluralize bug fix | Tasks 1, 2 |
| M3 reviewer recommendation: shared parse_id_from_url | Task 1 |
| M3 reviewer recommendation: workspace_filter integration test | Task 4 |
| M3 reviewer recommendation: document schema content_hash gap | Task 3 |

**Placeholder scan:** No "TBD", "TODO", "fill in", "similar to" patterns. Every code/command step is concrete.

**Type consistency check:**
- Codec function signatures: `write_<kind>(dir, slug, &<Type>) -> Result<Vec<u8>>` consistent across rule/label/engine/engine_field (Task 10) and matched by drivers (Task 11).
- Pull driver signatures: `pub async fn pull(ctx: &mut PullCtx<'_>) -> Result<usize>` consistent for the four new flat-list drivers (Task 11).
- API method names use Rossum's URL convention: `list_engine_fields` corresponds to `/api/v1/engine_fields` (underscore in URL); model module name is `engine_field` (Rust convention); directory is `engine-fields/` (kebab-case).
- `pluralize(n, singular, plural)` is referenced in Task 2 and Task 11 with the same signature defined in Task 1.
- Lockfile kinds: `"rules"`, `"labels"`, `"engines"`, `"engine_fields"` (underscore) consistent in Tasks 11 and 12.

**Scope check:** 13 tasks. Comparable to M3's 14 and M2's 16. Mostly pattern replication; the only novelty is the pluralize utility and a couple of small refactors. No technical debt accumulating.

---

## Next milestones

- **M5:** Workflows + workflow steps + email templates.
- **M6:** MDH dataset metadata + indexes.
- **M7:** Three-way merge + content_hash drift detection.
- **M8:** Conflict resolver TUI; indexer.
- **M9:** `rdc push`.
- **M10:** Overlays.
- **M11:** Mapping wizard, `rdc plan`, `rdc apply`.
- **M12:** Auxiliary commands.
- **M13:** Distribution.
