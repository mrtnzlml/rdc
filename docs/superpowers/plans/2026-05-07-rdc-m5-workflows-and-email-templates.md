# rdc M5 — Workflows, Workflow Steps, and Email Templates Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Apply the four M4 reviewer recommendations and add the three remaining flat-list object types per spec §11 — Workflows, Workflow Steps, Email Templates. After M5, only MDH dataset metadata + indexes remain (M6) before the pull side is feature-complete.

**Architecture:** Same model+codec+driver+API pattern. Each new object type gets a model, a codec, an API list method, and a flat-list pull driver. Refactors are applied first so they propagate to the new drivers automatically.

**Tech Stack:** Same as M4.

**What this milestone deliberately omits** (deferred):
- MDH dataset metadata + indexes (M6)
- Three-way merge, conflict resolver, push, plan/apply, overlays, mapping, distribution (M7+)

**End state of M5:**

```
$ rdc pull dev
Pulled 1 organization, 2 workspaces, 3 queues, 3 schemas, 1 inbox, 2 hooks,
1 rule, 2 labels, 1 engine, 2 engine fields, 1 workflow, 2 workflow steps,
1 email template from env 'dev'

$ ls envs/dev/
email-templates/  engines/  engine-fields/  hooks/  labels/  organization.json
rules/  workflows/  workflow-steps/  workspaces/
```

---

## File Structure

| Path | Status | Responsibility |
|---|---|---|
| `src/cli/pull/rules.rs` | Modify | Guard `create_dir_all` (only when items present) |
| `src/cli/pull/labels.rs` | Modify | Same guard |
| `src/cli/pull/engines.rs` | Modify | Same guard |
| `src/cli/pull/engine_fields.rs` | Modify | Same guard |
| `src/cli/pull/hooks.rs` | Modify | Same guard |
| `src/cli/pull/workspaces.rs` | Modify | Same guard |
| `src/api/mod.rs` | Modify | Update stale doc comment; add `list_workflows`, `list_workflow_steps`, `list_email_templates` |
| `src/snapshot/schema.rs` | Modify | Document combined-hash algorithm for M7 |
| `src/model/workflow.rs` | Create | `Workflow` struct |
| `src/model/workflow_step.rs` | Create | `WorkflowStep` struct |
| `src/model/email_template.rs` | Create | `EmailTemplate` struct |
| `src/model/mod.rs` | Modify | Re-export new types |
| `src/snapshot/workflow.rs` | Create | Workflow codec |
| `src/snapshot/workflow_step.rs` | Create | Workflow step codec |
| `src/snapshot/email_template.rs` | Create | Email template codec |
| `src/snapshot/mod.rs` | Modify | Declare new submodules |
| `src/paths.rs` | Modify | Add `workflows_dir`, `workflow_steps_dir`, `email_templates_dir` |
| `src/cli/pull/workflows.rs` | Create | Pull driver |
| `src/cli/pull/workflow_steps.rs` | Create | Pull driver |
| `src/cli/pull/email_templates.rs` | Create | Pull driver |
| `src/cli/pull/mod.rs` | Modify | Wire new drivers |
| `tests/cli_pull.rs` | Modify | Mocks + assertions for the three new kinds; tighten workspace_filter test |
| `tests/api.rs` | Modify | Tests for the three new list methods |
| `testdata/fixtures/workflows_list.json` | Create | Fixture |
| `testdata/fixtures/workflow_steps_list.json` | Create | Fixture |
| `testdata/fixtures/email_templates_list.json` | Create | Fixture |

---

## Task 1: Guard `create_dir_all` in flat-list drivers

The M4 reviewer flagged that all flat-list drivers create `<kind>/` unconditionally before the loop. Empty results produce empty dirs that clutter the working tree. Move the call inside the loop so it only runs when at least one object will be written.

**Files:**
- Modify: `src/cli/pull/hooks.rs`
- Modify: `src/cli/pull/rules.rs`
- Modify: `src/cli/pull/labels.rs`
- Modify: `src/cli/pull/engines.rs`
- Modify: `src/cli/pull/engine_fields.rs`
- Modify: `src/cli/pull/workspaces.rs`

- [ ] **Step 1: Update each driver**

For each of the six driver files, find the unconditional `std::fs::create_dir_all(...)` call that precedes the for loop and DELETE it. Then, inside the loop body, add a conditional creation of the parent dir using a `dir_created` flag. Pattern (apply consistently):

```rust
    let mut used: HashSet<String> = HashSet::new();
    let mut dir_created = false;
    for item in &items {
        if !dir_created {
            std::fs::create_dir_all(ctx.paths.<KIND>_dir())
                .with_context(|| format!("creating {}", ctx.paths.<KIND>_dir().display()))?;
            dir_created = true;
        }
        // ... existing body ...
    }
```

Apply to:
- `hooks.rs` → `hooks_dir()`
- `rules.rs` → `rules_dir()`
- `labels.rs` → `labels_dir()`
- `engines.rs` → `engines_dir()`
- `engine_fields.rs` → `engine_fields_dir()`
- `workspaces.rs` → `workspaces_dir()`

The queue driver in `queues.rs` already creates `queue_dir` per-queue inside its loop, so no change is needed there. The `organization` driver writes a single file, so no change needed.

- [ ] **Step 2: Run tests**

Run: `. "$HOME/.cargo/env" && cargo test`
Expected: 105 tests still pass (the existing fixtures all return non-empty results, so the dir IS created by the loop body's first iteration).

- [ ] **Step 3: Commit**

```bash
git add src/cli/pull/
git commit -m "refactor(cli): only create kind directories when objects are present"
```

---

## Task 2: Document schema combined-hash algorithm + update stale RossumClient doc

**Files:**
- Modify: `src/snapshot/schema.rs`
- Modify: `src/api/mod.rs`

- [ ] **Step 1: Expand the schema codec doc comment**

In `src/snapshot/schema.rs`, find the existing doc comment block above `pub fn write_schema`. Replace its content with:

```rust
/// Write a schema to `<queue_dir>/schema.json`, extracting any formula field
/// `formula` strings into `<queue_dir>/formulas/<field_id>.py` files.
/// Returns the JSON bytes written (for content_hash).
///
/// **Hash coverage gap:** The returned bytes are the post-extraction
/// `schema.json` content. Changes to extracted `formulas/*.py` files are NOT
/// reflected in the returned hash.
///
/// **Combined-hash algorithm for M7's three-way merge:** When implementing
/// drift detection, compute the canonical schema content hash as
/// `SHA-256(schema_json_bytes || 0x00 || formula_1_path || 0x00 ||
/// formula_1_bytes || 0x00 || formula_2_path || 0x00 || formula_2_bytes ||
/// ...)` where formulas are sorted by `field_id`. The 0x00 separator makes
/// boundaries unambiguous; sorting makes the hash deterministic across
/// platforms with non-stable filesystem listing order. Until M7, the
/// lockfile stores the simpler `schema.json`-only hash.
```

- [ ] **Step 2: Update RossumClient doc comment**

In `src/api/mod.rs`, find the doc comment block above `pub struct RossumClient`. Replace it with:

```rust
/// Rossum API client. Holds a base URL (e.g. `https://X.rossum.app/api/v1`)
/// and a static API token. Pagination is followed transparently for `list_*`
/// methods. As of M5, supports list/get for organizations, workspaces, queues,
/// inboxes, schemas, hooks, rules, labels, engines, engine fields, workflows,
/// workflow steps, and email templates.
```

- [ ] **Step 3: Build**

Run: `. "$HOME/.cargo/env" && cargo build`
Expected: clean.

- [ ] **Step 4: Commit**

```bash
git add src/
git commit -m "docs: schema combined-hash algorithm + refresh RossumClient scope comment"
```

---

## Task 3: `Workflow` model

**Files:**
- Create: `src/model/workflow.rs`

- [ ] **Step 1: Define struct + tests**

Create `src/model/workflow.rs`:

```rust
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;

/// Rossum workflow. Org-level orchestration for queue-to-queue transitions.
#[derive(Debug, Serialize, Deserialize, PartialEq, Clone)]
pub struct Workflow {
    pub id: u64,
    pub url: String,
    pub name: String,
    #[serde(default)]
    pub steps: Vec<String>,
    #[serde(flatten)]
    pub extra: BTreeMap<String, Value>,
}

impl Workflow {
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
            "id": 700,
            "url": "https://x.rossum.app/api/v1/workflows/700",
            "name": "AP Approval Flow",
            "steps": [
                "https://x.rossum.app/api/v1/workflow_steps/1",
                "https://x.rossum.app/api/v1/workflow_steps/2"
            ],
            "modified_at": "2026-04-20T08:00:00Z",
            "queue": "https://x.rossum.app/api/v1/queues/100"
        });
        let w: Workflow = serde_json::from_value(payload.clone()).unwrap();
        assert_eq!(w.id, 700);
        assert_eq!(w.steps.len(), 2);
        let round_trip = serde_json::to_value(&w).unwrap();
        assert_eq!(round_trip, payload);
    }

    #[test]
    fn missing_steps_defaults_to_empty() {
        let payload = json!({
            "id": 1,
            "url": "https://x/api/v1/workflows/1",
            "name": "W"
        });
        let w: Workflow = serde_json::from_value(payload).unwrap();
        assert!(w.steps.is_empty());
    }
}
```

(Module wiring at end of Task 5.)

---

## Task 4: `WorkflowStep` model

**Files:**
- Create: `src/model/workflow_step.rs`

- [ ] **Step 1: Define struct + tests**

Create `src/model/workflow_step.rs`:

```rust
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;

/// Rossum workflow step. Belongs to a workflow; defines one stage in a
/// queue-to-queue transition.
#[derive(Debug, Serialize, Deserialize, PartialEq, Clone)]
pub struct WorkflowStep {
    pub id: u64,
    pub url: String,
    pub name: String,
    pub workflow: String,
    #[serde(flatten)]
    pub extra: BTreeMap<String, Value>,
}

impl WorkflowStep {
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
            "id": 1,
            "url": "https://x.rossum.app/api/v1/workflow_steps/1",
            "name": "Manager Approval",
            "workflow": "https://x.rossum.app/api/v1/workflows/700",
            "modified_at": "2026-04-20T08:00:00Z",
            "step_type": "approval"
        });
        let s: WorkflowStep = serde_json::from_value(payload.clone()).unwrap();
        assert_eq!(s.id, 1);
        assert_eq!(s.workflow, "https://x.rossum.app/api/v1/workflows/700");
        let round_trip = serde_json::to_value(&s).unwrap();
        assert_eq!(round_trip, payload);
    }
}
```

---

## Task 5: `EmailTemplate` model

**Files:**
- Create: `src/model/email_template.rs`
- Modify: `src/model/mod.rs`

- [ ] **Step 1: Define struct + tests**

Create `src/model/email_template.rs`:

```rust
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;

/// Rossum email template. Used to customize notification emails.
#[derive(Debug, Serialize, Deserialize, PartialEq, Clone)]
pub struct EmailTemplate {
    pub id: u64,
    pub url: String,
    pub name: String,
    pub subject: String,
    #[serde(default)]
    pub queues: Vec<String>,
    #[serde(flatten)]
    pub extra: BTreeMap<String, Value>,
}

impl EmailTemplate {
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
            "id": 9001,
            "url": "https://x.rossum.app/api/v1/email_templates/9001",
            "name": "Rejection Notice",
            "subject": "Your invoice was rejected",
            "queues": ["https://x.rossum.app/api/v1/queues/100"],
            "modified_at": "2026-04-20T08:00:00Z",
            "body_template": "Hello,\n..."
        });
        let t: EmailTemplate = serde_json::from_value(payload.clone()).unwrap();
        assert_eq!(t.id, 9001);
        assert_eq!(t.subject, "Your invoice was rejected");
        let round_trip = serde_json::to_value(&t).unwrap();
        assert_eq!(round_trip, payload);
    }
}
```

- [ ] **Step 2: Wire all three into `model/mod.rs`**

Replace `src/model/mod.rs`:

```rust
pub mod email_template;
pub mod engine;
pub mod engine_field;
pub mod hook;
pub mod inbox;
pub mod label;
pub mod organization;
pub mod queue;
pub mod rule;
pub mod schema;
pub mod workflow;
pub mod workflow_step;
pub mod workspace;

pub use email_template::EmailTemplate;
pub use engine::Engine;
pub use engine_field::EngineField;
pub use hook::Hook;
pub use inbox::Inbox;
pub use label::Label;
pub use organization::Organization;
pub use queue::Queue;
pub use rule::Rule;
pub use schema::Schema;
pub use workflow::Workflow;
pub use workflow_step::WorkflowStep;
pub use workspace::Workspace;
```

- [ ] **Step 3: Run model tests**

Run: `. "$HOME/.cargo/env" && cargo test --lib model`
Expected: model tests pass — adds 4 new (Workflow round_trip + missing_steps_defaults, WorkflowStep round_trip, EmailTemplate round_trip).

- [ ] **Step 4: Commit Tasks 3 + 4 + 5 together**

```bash
git add src/model/
git commit -m "feat(model): add Workflow, WorkflowStep, EmailTemplate types"
```

---

## Task 6: API methods + fixtures + tests

**Files:**
- Modify: `src/api/mod.rs`
- Modify: `tests/api.rs`
- Create: `testdata/fixtures/workflows_list.json`
- Create: `testdata/fixtures/workflow_steps_list.json`
- Create: `testdata/fixtures/email_templates_list.json`

- [ ] **Step 1: Create fixtures**

Create `testdata/fixtures/workflows_list.json`:

```json
{
  "pagination": { "total": 1, "total_pages": 1, "next": null, "previous": null },
  "results": [
    {
      "id": 700,
      "url": "https://mock.rossum.app/api/v1/workflows/700",
      "name": "AP Approval Flow",
      "steps": [
        "https://mock.rossum.app/api/v1/workflow_steps/1",
        "https://mock.rossum.app/api/v1/workflow_steps/2"
      ],
      "modified_at": "2026-04-20T08:00:00Z"
    }
  ]
}
```

Create `testdata/fixtures/workflow_steps_list.json`:

```json
{
  "pagination": { "total": 2, "total_pages": 1, "next": null, "previous": null },
  "results": [
    {
      "id": 1,
      "url": "https://mock.rossum.app/api/v1/workflow_steps/1",
      "name": "Manager Approval",
      "workflow": "https://mock.rossum.app/api/v1/workflows/700",
      "modified_at": "2026-04-20T08:00:00Z"
    },
    {
      "id": 2,
      "url": "https://mock.rossum.app/api/v1/workflow_steps/2",
      "name": "Finance Approval",
      "workflow": "https://mock.rossum.app/api/v1/workflows/700",
      "modified_at": "2026-04-20T08:00:00Z"
    }
  ]
}
```

Create `testdata/fixtures/email_templates_list.json`:

```json
{
  "pagination": { "total": 1, "total_pages": 1, "next": null, "previous": null },
  "results": [
    {
      "id": 9001,
      "url": "https://mock.rossum.app/api/v1/email_templates/9001",
      "name": "Rejection Notice",
      "subject": "Your invoice was rejected",
      "queues": ["https://mock.rossum.app/api/v1/queues/100"],
      "modified_at": "2026-04-20T08:00:00Z"
    }
  ]
}
```

- [ ] **Step 2: Add failing tests**

Append to `tests/api.rs`:

```rust
#[tokio::test]
async fn list_workflows_returns_workflows() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/v1/workflows"))
        .respond_with(ResponseTemplate::new(200).set_body_json(fixture("workflows_list.json")))
        .mount(&server)
        .await;
    let client = RossumClient::new(format!("{}/api/v1", server.uri()), "TEST_TOKEN".into()).unwrap();
    let workflows = client.list_workflows().await.unwrap();
    assert_eq!(workflows.len(), 1);
    assert_eq!(workflows[0].name, "AP Approval Flow");
}

#[tokio::test]
async fn list_workflow_steps_returns_steps() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/v1/workflow_steps"))
        .respond_with(ResponseTemplate::new(200).set_body_json(fixture("workflow_steps_list.json")))
        .mount(&server)
        .await;
    let client = RossumClient::new(format!("{}/api/v1", server.uri()), "TEST_TOKEN".into()).unwrap();
    let steps = client.list_workflow_steps().await.unwrap();
    assert_eq!(steps.len(), 2);
    assert_eq!(steps[1].name, "Finance Approval");
}

#[tokio::test]
async fn list_email_templates_returns_templates() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/v1/email_templates"))
        .respond_with(ResponseTemplate::new(200).set_body_json(fixture("email_templates_list.json")))
        .mount(&server)
        .await;
    let client = RossumClient::new(format!("{}/api/v1", server.uri()), "TEST_TOKEN".into()).unwrap();
    let templates = client.list_email_templates().await.unwrap();
    assert_eq!(templates.len(), 1);
    assert_eq!(templates[0].subject, "Your invoice was rejected");
}
```

- [ ] **Step 3: Implement API methods**

In `src/api/mod.rs`, append three methods inside the `impl RossumClient { ... }` block (after `list_engine_fields`):

```rust
    pub async fn list_workflows(&self) -> Result<Vec<crate::model::Workflow>> {
        let mut url = format!("{}/workflows", self.base_url);
        let mut out = Vec::new();
        loop {
            let page: Page<crate::model::Workflow> = self.get_json(&url).await?;
            out.extend(page.results);
            match page.pagination.next {
                Some(next) => url = next,
                None => break,
            }
        }
        Ok(out)
    }

    pub async fn list_workflow_steps(&self) -> Result<Vec<crate::model::WorkflowStep>> {
        let mut url = format!("{}/workflow_steps", self.base_url);
        let mut out = Vec::new();
        loop {
            let page: Page<crate::model::WorkflowStep> = self.get_json(&url).await?;
            out.extend(page.results);
            match page.pagination.next {
                Some(next) => url = next,
                None => break,
            }
        }
        Ok(out)
    }

    pub async fn list_email_templates(&self) -> Result<Vec<crate::model::EmailTemplate>> {
        let mut url = format!("{}/email_templates", self.base_url);
        let mut out = Vec::new();
        loop {
            let page: Page<crate::model::EmailTemplate> = self.get_json(&url).await?;
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
Expected: 14 tests pass (11 from M4 + 3 new).

- [ ] **Step 5: Commit**

```bash
git add src/api/ tests/ testdata/
git commit -m "feat(api): add list_workflows, list_workflow_steps, list_email_templates"
```

---

## Task 7: Snapshot codecs for the three new kinds

**Files:**
- Create: `src/snapshot/workflow.rs`
- Create: `src/snapshot/workflow_step.rs`
- Create: `src/snapshot/email_template.rs`
- Modify: `src/snapshot/mod.rs`

- [ ] **Step 1: Create `src/snapshot/workflow.rs`**

```rust
use crate::model::Workflow;
use crate::snapshot::writer::write_atomic;
use anyhow::{Context, Result};
use std::path::Path;

/// Write a workflow as `<dir>/<slug>.json`. Returns the bytes written.
pub fn write_workflow(dir: &Path, slug: &str, w: &Workflow) -> Result<Vec<u8>> {
    let path = dir.join(format!("{slug}.json"));
    let bytes = serde_json::to_vec_pretty(w).context("serializing workflow")?;
    let mut bytes = bytes;
    bytes.push(b'\n');
    write_atomic(&path, &bytes)?;
    Ok(bytes)
}

pub fn read_workflow(dir: &Path, slug: &str) -> Result<Workflow> {
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

    fn sample() -> Workflow {
        let v = json!({
            "id": 1,
            "url": "https://x/api/v1/workflows/1",
            "name": "W",
            "steps": []
        });
        serde_json::from_value(v).unwrap()
    }

    #[test]
    fn round_trip() {
        let dir = TempDir::new().unwrap();
        let original = sample();
        write_workflow(dir.path(), "w", &original).unwrap();
        let read = read_workflow(dir.path(), "w").unwrap();
        assert_eq!(original, read);
    }
}
```

- [ ] **Step 2: Create `src/snapshot/workflow_step.rs`**

```rust
use crate::model::WorkflowStep;
use crate::snapshot::writer::write_atomic;
use anyhow::{Context, Result};
use std::path::Path;

/// Write a workflow step as `<dir>/<slug>.json`. Returns the bytes written.
pub fn write_workflow_step(dir: &Path, slug: &str, s: &WorkflowStep) -> Result<Vec<u8>> {
    let path = dir.join(format!("{slug}.json"));
    let bytes = serde_json::to_vec_pretty(s).context("serializing workflow step")?;
    let mut bytes = bytes;
    bytes.push(b'\n');
    write_atomic(&path, &bytes)?;
    Ok(bytes)
}

pub fn read_workflow_step(dir: &Path, slug: &str) -> Result<WorkflowStep> {
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

    fn sample() -> WorkflowStep {
        let v = json!({
            "id": 1,
            "url": "https://x/api/v1/workflow_steps/1",
            "name": "S",
            "workflow": "https://x/api/v1/workflows/1"
        });
        serde_json::from_value(v).unwrap()
    }

    #[test]
    fn round_trip() {
        let dir = TempDir::new().unwrap();
        let original = sample();
        write_workflow_step(dir.path(), "s", &original).unwrap();
        let read = read_workflow_step(dir.path(), "s").unwrap();
        assert_eq!(original, read);
    }
}
```

- [ ] **Step 3: Create `src/snapshot/email_template.rs`**

```rust
use crate::model::EmailTemplate;
use crate::snapshot::writer::write_atomic;
use anyhow::{Context, Result};
use std::path::Path;

/// Write an email template as `<dir>/<slug>.json`. Returns the bytes written.
pub fn write_email_template(dir: &Path, slug: &str, t: &EmailTemplate) -> Result<Vec<u8>> {
    let path = dir.join(format!("{slug}.json"));
    let bytes = serde_json::to_vec_pretty(t).context("serializing email template")?;
    let mut bytes = bytes;
    bytes.push(b'\n');
    write_atomic(&path, &bytes)?;
    Ok(bytes)
}

pub fn read_email_template(dir: &Path, slug: &str) -> Result<EmailTemplate> {
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

    fn sample() -> EmailTemplate {
        let v = json!({
            "id": 1,
            "url": "https://x/api/v1/email_templates/1",
            "name": "T",
            "subject": "Subj",
            "queues": []
        });
        serde_json::from_value(v).unwrap()
    }

    #[test]
    fn round_trip() {
        let dir = TempDir::new().unwrap();
        let original = sample();
        write_email_template(dir.path(), "t", &original).unwrap();
        let read = read_email_template(dir.path(), "t").unwrap();
        assert_eq!(original, read);
    }
}
```

- [ ] **Step 4: Wire into `src/snapshot/mod.rs`**

Replace its content with:

```rust
pub mod email_template;
pub mod engine;
pub mod engine_field;
pub mod hook;
pub mod inbox;
pub mod label;
pub mod organization;
pub mod queue;
pub mod rule;
pub mod schema;
pub mod workflow;
pub mod workflow_step;
pub mod workspace;
pub mod writer;
```

- [ ] **Step 5: Run snapshot tests**

Run: `. "$HOME/.cargo/env" && cargo test --lib snapshot`
Expected: 30 snapshot tests pass (27 from M4 + 3 new round_trips).

- [ ] **Step 6: Commit**

```bash
git add src/snapshot/
git commit -m "feat(snapshot): workflow, workflow_step, email_template codecs"
```

---

## Task 8: Pull drivers + paths accessors

**Files:**
- Modify: `src/paths.rs`
- Create: `src/cli/pull/workflows.rs`
- Create: `src/cli/pull/workflow_steps.rs`
- Create: `src/cli/pull/email_templates.rs`
- Modify: `src/cli/pull/mod.rs`

- [ ] **Step 1: Add path accessors**

In `src/paths.rs`, add inside `impl Paths` (before its closing `}`):

```rust
    /// `<root>/envs/<env>/workflows/`
    pub fn workflows_dir(&self) -> PathBuf {
        self.env_root().join("workflows")
    }

    /// `<root>/envs/<env>/workflow-steps/`
    pub fn workflow_steps_dir(&self) -> PathBuf {
        self.env_root().join("workflow-steps")
    }

    /// `<root>/envs/<env>/email-templates/`
    pub fn email_templates_dir(&self) -> PathBuf {
        self.env_root().join("email-templates")
    }
```

In the `mod tests` block, add tests before the closing `}`:

```rust
    #[test]
    fn workflows_dir_path() {
        assert_eq!(p().workflows_dir(), Path::new("/proj/envs/dev/workflows"));
    }

    #[test]
    fn workflow_steps_dir_path() {
        assert_eq!(p().workflow_steps_dir(), Path::new("/proj/envs/dev/workflow-steps"));
    }

    #[test]
    fn email_templates_dir_path() {
        assert_eq!(p().email_templates_dir(), Path::new("/proj/envs/dev/email-templates"));
    }
```

- [ ] **Step 2: Create `src/cli/pull/workflows.rs`**

```rust
use super::common::{hash_for_lockfile, record_object, PullCtx};
use crate::slug::slugify_unique;
use crate::snapshot::workflow::write_workflow;
use anyhow::{Context, Result};
use std::collections::HashSet;

/// Pull all workflows. Returns the count.
pub async fn pull(ctx: &mut PullCtx<'_>) -> Result<usize> {
    let workflows = ctx.client.list_workflows().await.context("listing workflows")?;

    let mut used: HashSet<String> = HashSet::new();
    let mut dir_created = false;
    for w in &workflows {
        if !dir_created {
            std::fs::create_dir_all(ctx.paths.workflows_dir())
                .with_context(|| format!("creating {}", ctx.paths.workflows_dir().display()))?;
            dir_created = true;
        }
        let slug = slugify_unique(&w.name, &used);
        used.insert(slug.clone());

        let bytes = write_workflow(&ctx.paths.workflows_dir(), &slug, w)
            .with_context(|| format!("writing workflow '{}' to disk", w.name))?;
        let hash = hash_for_lockfile(&bytes);

        record_object(
            ctx.lockfile,
            "workflows",
            &slug,
            w.id,
            Some(w.url.clone()),
            w.modified_at().map(|s| s.to_string()),
            Some(hash),
        );
    }

    Ok(workflows.len())
}
```

- [ ] **Step 3: Create `src/cli/pull/workflow_steps.rs`**

```rust
use super::common::{hash_for_lockfile, record_object, PullCtx};
use crate::slug::slugify_unique;
use crate::snapshot::workflow_step::write_workflow_step;
use anyhow::{Context, Result};
use std::collections::HashSet;

/// Pull all workflow steps. Returns the count.
pub async fn pull(ctx: &mut PullCtx<'_>) -> Result<usize> {
    let steps = ctx.client.list_workflow_steps().await.context("listing workflow steps")?;

    let mut used: HashSet<String> = HashSet::new();
    let mut dir_created = false;
    for s in &steps {
        if !dir_created {
            std::fs::create_dir_all(ctx.paths.workflow_steps_dir())
                .with_context(|| format!("creating {}", ctx.paths.workflow_steps_dir().display()))?;
            dir_created = true;
        }
        let slug = slugify_unique(&s.name, &used);
        used.insert(slug.clone());

        let bytes = write_workflow_step(&ctx.paths.workflow_steps_dir(), &slug, s)
            .with_context(|| format!("writing workflow step '{}' to disk", s.name))?;
        let hash = hash_for_lockfile(&bytes);

        record_object(
            ctx.lockfile,
            "workflow_steps",
            &slug,
            s.id,
            Some(s.url.clone()),
            s.modified_at().map(|x| x.to_string()),
            Some(hash),
        );
    }

    Ok(steps.len())
}
```

- [ ] **Step 4: Create `src/cli/pull/email_templates.rs`**

```rust
use super::common::{hash_for_lockfile, record_object, PullCtx};
use crate::slug::slugify_unique;
use crate::snapshot::email_template::write_email_template;
use anyhow::{Context, Result};
use std::collections::HashSet;

/// Pull all email templates. Returns the count.
pub async fn pull(ctx: &mut PullCtx<'_>) -> Result<usize> {
    let templates = ctx.client.list_email_templates().await.context("listing email templates")?;

    let mut used: HashSet<String> = HashSet::new();
    let mut dir_created = false;
    for t in &templates {
        if !dir_created {
            std::fs::create_dir_all(ctx.paths.email_templates_dir())
                .with_context(|| format!("creating {}", ctx.paths.email_templates_dir().display()))?;
            dir_created = true;
        }
        let slug = slugify_unique(&t.name, &used);
        used.insert(slug.clone());

        let bytes = write_email_template(&ctx.paths.email_templates_dir(), &slug, t)
            .with_context(|| format!("writing email template '{}' to disk", t.name))?;
        let hash = hash_for_lockfile(&bytes);

        record_object(
            ctx.lockfile,
            "email_templates",
            &slug,
            t.id,
            Some(t.url.clone()),
            t.modified_at().map(|s| s.to_string()),
            Some(hash),
        );
    }

    Ok(templates.len())
}
```

- [ ] **Step 5: Wire into orchestrator**

Replace `src/cli/pull/mod.rs`:

```rust
use crate::api::RossumClient;
use crate::config::ProjectConfig;
use crate::paths::Paths;
use crate::secrets::resolve_token;
use crate::state::Lockfile;
use anyhow::{anyhow, Context, Result};

mod common;
mod email_templates;
mod engine_fields;
mod engines;
mod hooks;
mod labels;
mod organization;
mod queues;
mod rules;
mod workflow_steps;
mod workflows;
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
    let n_workflows = workflows::pull(&mut ctx).await
        .with_context(|| format!("pulling workflows for env '{env}'"))?;
    let n_workflow_steps = workflow_steps::pull(&mut ctx).await
        .with_context(|| format!("pulling workflow steps for env '{env}'"))?;
    let n_email_templates = email_templates::pull(&mut ctx).await
        .with_context(|| format!("pulling email templates for env '{env}'"))?;

    lockfile.save(&paths.lockfile())?;
    println!(
        "Pulled {}, {}, {}, {}, {}, {}, {}, {}, {}, {}, {}, {}, {} from env '{env}'",
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
        common::pluralize(n_workflows, "workflow", "workflows"),
        common::pluralize(n_workflow_steps, "workflow step", "workflow steps"),
        common::pluralize(n_email_templates, "email template", "email templates"),
    );
    Ok(())
}
```

- [ ] **Step 6: Run lib tests**

Run: `. "$HOME/.cargo/env" && cargo test --lib`
Expected: lib tests pass.

- [ ] **Step 7: Commit**

```bash
git add src/paths.rs src/cli/pull/
git commit -m "feat(cli): pull workflows, workflow_steps, email_templates"
```

---

## Task 9: Integration test extension + tighten workspace_filter test

**Files:**
- Modify: `tests/cli_pull.rs`

- [ ] **Step 1: Extend `pull_writes_full_workspace_tree` with the three new mocks**

In `tests/cli_pull.rs`, find the `pull_writes_full_workspace_tree` test. Add three new mocks immediately after the engine_fields mock:

```rust
    Mock::given(method("GET"))
        .and(path("/api/v1/workflows"))
        .respond_with(ResponseTemplate::new(200).set_body_json(fixture("workflows_list.json")))
        .mount(&server)
        .await;

    Mock::given(method("GET"))
        .and(path("/api/v1/workflow_steps"))
        .respond_with(ResponseTemplate::new(200).set_body_json(fixture("workflow_steps_list.json")))
        .mount(&server)
        .await;

    Mock::given(method("GET"))
        .and(path("/api/v1/email_templates"))
        .respond_with(ResponseTemplate::new(200).set_body_json(fixture("email_templates_list.json")))
        .mount(&server)
        .await;
```

Append three new stdout assertions to the existing chain:

```rust
        .stdout(predicate::str::contains("1 workflow"))
        .stdout(predicate::str::contains("2 workflow steps"))
        .stdout(predicate::str::contains("1 email template"));
```

After the `assert!(lf.contains("\"engine_fields\""));` line, add:

```rust
    // M5 kinds present
    assert!(env_root.join("workflows/ap-approval-flow.json").exists());
    assert!(env_root.join("workflow-steps/manager-approval.json").exists());
    assert!(env_root.join("workflow-steps/finance-approval.json").exists());
    assert!(env_root.join("email-templates/rejection-notice.json").exists());

    // Lockfile records new kinds
    assert!(lf.contains("\"workflows\""));
    assert!(lf.contains("\"workflow_steps\""));
    assert!(lf.contains("\"email_templates\""));
```

- [ ] **Step 2: Add the three new mocks to `pull_with_workspace_filter_skips_non_matching` too**

In the same file, find that test and add the three new mocks (same code as Step 1) inside it.

- [ ] **Step 3: Tighten `pull_with_workspace_filter_skips_non_matching`**

The M4 reviewer suggested asserting via parsed lockfile instead of just `contains`. Replace the closing assertions block (after `let lf = ...`) with:

```rust
    let lf = std::fs::read_to_string(project.path().join(".rdc/state/dev.lock.json")).unwrap();
    let lf_value: serde_json::Value = serde_json::from_str(&lf).unwrap();

    // Exactly one workspace recorded.
    let ws_obj = lf_value["objects"]["workspaces"].as_object().unwrap();
    assert_eq!(ws_obj.len(), 1, "expected 1 workspace, got {}: {:?}", ws_obj.len(), ws_obj.keys().collect::<Vec<_>>());
    assert!(ws_obj.contains_key("invoices-ap"));

    // Queues from the filtered workspace are skipped; only "Cost Invoices" and
    // "Credit Notes" (both belonging to "Invoices AP") should appear.
    let q_obj = lf_value["objects"]["queues"].as_object().unwrap();
    assert_eq!(q_obj.len(), 2, "expected 2 queues, got {}: {:?}", q_obj.len(), q_obj.keys().collect::<Vec<_>>());
    assert!(q_obj.contains_key("cost-invoices"));
    assert!(q_obj.contains_key("credit-notes"));
    assert!(!q_obj.contains_key("purchase-orders"));
```

This survives summary-line format changes and explicitly proves the lockfile state.

- [ ] **Step 4: Run all tests**

Run: `. "$HOME/.cargo/env" && cargo test`
Expected: ALL tests pass.

- [ ] **Step 5: Commit**

```bash
git add tests/cli_pull.rs
git commit -m "test(cli): integration test for workflows + tighten workspace_filter assertions"
```

---

## Task 10: README update

**Files:**
- Modify: `README.md`

- [ ] **Step 1: Update**

Replace `README.md`:

````
# rdc

`rdc` (Rossum Deployment as Code) snapshots Rossum.ai configurations to disk for AI-assisted local development and deploys them across environments.

**Status:** M5. `rdc init` and `rdc pull <env>` cover everything except MDH: organizations, workspaces (with optional regex filter), queues, schemas (with formula extraction), inboxes, hooks, rules, labels, engines, engine fields, workflows, workflow steps, and email templates. See `docs/superpowers/specs/2026-05-06-rdc-design.md` for the full design and `docs/superpowers/plans/` for implementation plans.

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
# email-templates/  engines/  engine-fields/  hooks/  labels/  organization.json
# rules/  workflows/  workflow-steps/  workspaces/
```

## Tests

```sh
cargo test
```
````

- [ ] **Step 2: Commit**

```bash
git add README.md
git commit -m "docs: update README for M5 scope"
```

---

## Self-Review

**Spec coverage:**
- §11 Workflows / Workflow steps / Email templates → Tasks 3-9
- §11 MDH → deferred to M6
- M4 reviewer recommendations: empty-dir guard (Task 1), schema combined-hash docs (Task 2), RossumClient stale doc (Task 2), tighten workspace_filter test (Task 9)

**Placeholder scan:** No "TBD"/"TODO"/"similar to" patterns.

**Type consistency:** API URLs use underscore (`workflow_steps`, `email_templates`) per Rossum convention; directories use kebab-case (`workflow-steps`, `email-templates`); Rust modules use snake_case. Lockfile kinds use underscore. The naming dance is consistent with M4 (engine_fields).

**Scope check:** 10 tasks. Pure pattern replication after M4. No novel architecture.

---

## Next milestones

- **M6:** MDH dataset metadata + indexes (no row data per spec).
- **M7:** Three-way merge + content_hash drift detection (consumes M2 lockfile fields + M5 schema-combined-hash algorithm).
- **M8:** Conflict resolver TUI + indexer (`_index.md`).
- **M9:** `rdc push`.
- **M10:** Overlays.
- **M11:** Mapping wizard + `rdc plan` + `rdc apply`.
- **M12:** Auxiliary commands (status, diff, auth, repair).
- **M13:** Distribution.
